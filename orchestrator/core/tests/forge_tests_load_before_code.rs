// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The tests are loaded before CODE (AEGIS ADR-141, Update of 2026-10-16
//! `tests-load-before-code-2026-10-16`, clause 1 and Trigger (a)).
//!
//! Run `e7f2c2aa` passed TEST with two test files that did not parse
//! (`test/test_readme_section.py`, a `SyntaxError` at line 52), and CODE spent
//! its 900 s try on a suite no implementation could pass. After TEST the
//! Forge now runs the suite once, as `CHECK_TESTS`, with EXECUTE_TESTS's image
//! and command: a suite whose tester-written file does not load goes back to
//! TEST under "# The tests did not load", within TEST's three visits, and any
//! other run goes to CODE as "# The last run of the suite".
//!
//! The classification runs inside the step's container, in the state's own
//! shell command: the step's answer carries only its exit code and its two
//! streams, so a load failure is told to the workflow by a reserved exit code
//! the `exit_code` transition reads. The tests below run that command, read
//! from the template, under `sh` against the suite outputs Python's unittest
//! and pytest print.

use aegis_orchestrator_core::domain::agent::{
    ExecutionMode, ExecutionStrategy, ImagePullPolicy, ValidatorSpec,
};
use aegis_orchestrator_core::domain::execution::{ExecutionId, ExecutionInput, ExhaustedBy};
use aegis_orchestrator_core::domain::runtime::{
    AgentRuntime, InstanceId, InstanceStatus, ResourceLimits, RuntimeConfig, RuntimeError,
    TaskInput, TaskOutput,
};
use aegis_orchestrator_core::domain::supervisor::{
    two_tries_bound, Supervisor, SupervisorObserver,
};
use aegis_orchestrator_core::domain::validation::ValidationResults;
use aegis_orchestrator_core::domain::workflow::{StateKind, Workflow, WorkflowState};
use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;
use async_trait::async_trait;
use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// The exit code `CHECK_TESTS` ends with when the tests did not load.
const DID_NOT_LOAD: i32 = 86;

fn forge() -> Workflow {
    let yaml = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../cli/templates/workflows/the-forge.yaml"
    ))
    .expect("the Forge's template is readable");
    WorkflowParser::parse_yaml(&yaml)
        .unwrap_or_else(|e| panic!("the orchestrator's parser refuses the-forge: {e}"))
}

fn state<'a>(workflow: &'a Workflow, name: &str) -> Option<&'a WorkflowState> {
    workflow
        .spec
        .states
        .iter()
        .find(|(n, _)| n.as_str() == name)
        .map(|(_, s)| s)
}

/// A transition as `<condition json> -> <target>`, in order.
fn routes(state: &WorkflowState) -> Vec<String> {
    state
        .transitions
        .iter()
        .map(|t| {
            format!(
                "{} -> {}",
                serde_json::to_value(&t.condition).unwrap(),
                t.target.as_str()
            )
        })
        .collect()
}

fn agent_input<'a>(workflow: &'a Workflow, name: &str) -> &'a str {
    match state(workflow, name).map(|s| &s.kind) {
        Some(StateKind::Agent { input, .. }) => input,
        _ => panic!("the-forge has no Agent state {name}"),
    }
}

/// `CHECK_TESTS` follows TEST and runs the suite as EXECUTE_TESTS does; a
/// load failure goes back to TEST, within its three visits, and anything else
/// to CODE; TEST is told why it is back, and CODE reads the run as it reads
/// EXECUTE_TESTS's.
#[test]
fn check_tests_follows_test_and_routes_a_load_failure_to_test_and_the_rest_to_code() {
    let workflow = forge();
    let mut complaints = Vec::new();

    let test = state(&workflow, "TEST").expect("the-forge has TEST");
    let expected_test_routes = vec![
        "{\"condition\":\"on_success\"} -> CHECK_TESTS".to_string(),
        "{\"condition\":\"on_failure\"} -> FAILED".to_string(),
    ];
    if routes(test) != expected_test_routes {
        complaints.push(format!("TEST routes {:#?}", routes(test)));
    }
    if test.max_state_visits != Some(3) {
        complaints.push(format!(
            "TEST's max_state_visits is {:?}, not 3",
            test.max_state_visits
        ));
    }

    match state(&workflow, "CHECK_TESTS") {
        None => complaints.push("the-forge has no state CHECK_TESTS".to_string()),
        Some(check) => {
            let expected = vec![
                format!("{{\"condition\":\"exit_code\",\"value\":{DID_NOT_LOAD}}} -> TEST"),
                "{\"condition\":\"always\"} -> CODE".to_string(),
            ];
            if routes(check) != expected {
                complaints.push(format!("CHECK_TESTS routes {:#?}", routes(check)));
            }
            let (
                Some(StateKind::ContainerRun {
                    image: check_image,
                    env: check_env,
                    workdir: check_workdir,
                    volumes: check_volumes,
                    shell: check_shell,
                    network_mode: check_network,
                    command: check_command,
                    ..
                }),
                Some(StateKind::ContainerRun {
                    image,
                    env,
                    workdir,
                    volumes,
                    network_mode,
                    ..
                }),
            ) = (
                Some(&check.kind),
                state(&workflow, "EXECUTE_TESTS").map(|s| &s.kind),
            )
            else {
                panic!("CHECK_TESTS and EXECUTE_TESTS are both ContainerRun states");
            };
            if check_image != image {
                complaints.push(format!(
                    "CHECK_TESTS runs the image {check_image}, EXECUTE_TESTS {image}"
                ));
            }
            if check_env.get("FORGE_TEST_COMMAND") != env.get("FORGE_TEST_COMMAND") {
                complaints.push(format!(
                    "CHECK_TESTS's suite is {:?}, EXECUTE_TESTS's {:?}",
                    check_env.get("FORGE_TEST_COMMAND"),
                    env.get("FORGE_TEST_COMMAND")
                ));
            }
            if check_workdir != workdir
                || check_network != network_mode
                || check_volumes
                    .iter()
                    .map(|v| (&v.name, &v.mount_path))
                    .collect::<Vec<_>>()
                    != volumes
                        .iter()
                        .map(|v| (&v.name, &v.mount_path))
                        .collect::<Vec<_>>()
            {
                complaints.push(
                    "CHECK_TESTS does not mount and reach the clone as EXECUTE_TESTS does"
                        .to_string(),
                );
            }
            if !*check_shell {
                complaints.push("CHECK_TESTS is not shell: true".to_string());
            }
            if check_command.join(" ").contains("{{") {
                complaints.push(
                    "CHECK_TESTS's command holds a template expression: the worker renders it"
                        .to_string(),
                );
            }
        }
    }

    let test_input = agent_input(&workflow, "TEST");
    for line in [
        "{{#if CHECK_TESTS}}",
        "# The tests did not load",
        "{{{CHECK_TESTS.output.stderr}}}",
        "{{{CHECK_TESTS.output.stdout}}}",
    ] {
        if !test_input.lines().any(|l| l == line) {
            complaints.push(format!("TEST's input has no line {line:?}"));
        }
    }
    let code_input = agent_input(&workflow, "CODE");
    if code_input.matches("# The last run of the suite").count() != 2 {
        complaints.push(
            "CODE's input does not read the last run from EXECUTE_TESTS, or else CHECK_TESTS"
                .to_string(),
        );
    }
    for line in [
        "Exit code {{CHECK_TESTS.output.exit_code}}.",
        "{{{CHECK_TESTS.output.stderr}}}",
        "{{{CHECK_TESTS.output.stdout}}}",
    ] {
        if !code_input.lines().any(|l| l == line) {
            complaints.push(format!("CODE's input has no line {line:?}"));
        }
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// `CHECK_TESTS`'s shell command and the suite's variable it reads.
fn check_command(workflow: &Workflow) -> (String, String) {
    match state(workflow, "CHECK_TESTS").map(|s| &s.kind) {
        Some(StateKind::ContainerRun { command, env, .. }) => {
            let variable = env
                .iter()
                .find(|(_, v)| v.as_str() == "{{{input.test_command}}}")
                .map(|(k, _)| k.clone())
                .expect("CHECK_TESTS gives its shell the input's test_command");
            (command.join(" "), variable)
        }
        _ => panic!("the-forge has no ContainerRun state CHECK_TESTS"),
    }
}

/// What `CHECK_TESTS` did with one run of a suite.
struct Checked {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

/// Runs `CHECK_TESTS`'s command as the step does (`sh -c`, in the clone's
/// root), with a suite that prints `stdout` and `stderr` and exits `code`;
/// `@REPO@` in the outputs is the clone's root.
fn check(stdout: &str, stderr: &str, code: i32) -> Checked {
    let workflow = forge();
    let (command, variable) = check_command(&workflow);
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path().canonicalize().unwrap();
    let outputs = tempfile::tempdir().unwrap();
    let (out, err) = (
        outputs.path().join("suite.out"),
        outputs.path().join("suite.err"),
    );
    let at_root = |s: &str| s.replace("@REPO@", &root.to_string_lossy());
    std::fs::write(&out, at_root(stdout)).unwrap();
    std::fs::write(&err, at_root(stderr)).unwrap();
    let suite = format!(
        "cat '{}'; cat '{}' >&2; exit {code}",
        out.display(),
        err.display()
    );
    let env: HashMap<&str, String> = [(variable.as_str(), suite)].into_iter().collect();
    let ran = Command::new("sh")
        .arg("-c")
        .arg(&command)
        .current_dir(&root)
        .env("PWD", &root)
        .env("TMPDIR", outputs.path())
        .envs(env)
        .output()
        .expect("sh runs");
    let leftovers: Vec<_> = std::fs::read_dir(outputs.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n != "suite.out" && n != "suite.err")
        .collect();
    assert!(
        leftovers.is_empty(),
        "CHECK_TESTS left files behind: {leftovers:?}"
    );
    Checked {
        exit_code: ran.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&ran.stdout).to_string(),
        stderr: String::from_utf8_lossy(&ran.stderr).to_string(),
    }
}

/// This run's suite, `python -m unittest discover -s test -v`, on the
/// tester's `test/test_readme_section.py` as TEST answered it: unittest's
/// `_FailedTest` and a `SyntaxError` at line 52 of the tester's file. The
/// tests did not load; the run goes back to TEST with the suite's output.
#[test]
fn this_runs_unittest_output_is_a_load_failure() {
    let checked = check("", UNITTEST_SYNTAX_STDERR, 1);
    assert_eq!(
        checked.exit_code, DID_NOT_LOAD,
        "the run's output was not read as a load failure: {}",
        checked.stderr
    );
    assert!(
        checked
            .stderr
            .contains("test/test_readme_section.py\", line 52"),
        "the suite's output does not reach TEST: {:?}",
        checked.stderr
    );
}

/// The same file under pytest: "ERROR collecting" and the `SyntaxError`.
#[test]
fn pytest_collecting_a_file_that_does_not_parse_is_a_load_failure() {
    let checked = check(PYTEST_SYNTAX_STDOUT, "", 2);
    assert_eq!(
        checked.exit_code, DID_NOT_LOAD,
        "pytest's collection error was not read as a load failure: {}",
        checked.stdout
    );
    assert!(checked
        .stdout
        .contains("ERROR collecting test/test_readme_section.py"));
}

/// A guard: a test red because the code it imports is not written yet loads
/// as far as the tester can make it; that is CODE's to write, so the run goes
/// to CODE with the suite's own exit code, under unittest and under pytest.
#[test]
fn a_test_importing_code_not_written_yet_goes_to_code() {
    let unittest = check("", UNITTEST_MISSING_MODULE_STDERR, 1);
    assert_eq!(
        unittest.exit_code, 1,
        "unittest's missing implementation module went to TEST: {}",
        unittest.stderr
    );
    let pytest = check(PYTEST_MISSING_MODULE_STDOUT, "", 2);
    assert_eq!(
        pytest.exit_code, 2,
        "pytest's missing implementation module went to TEST: {}",
        pytest.stdout
    );
}

/// A guard: a suite that loads goes to CODE with its own exit code and its
/// output; a suite whose own exit code is the reserved one is told to CODE
/// as 1, never as a load failure.
#[test]
fn a_suite_that_loads_goes_to_code_with_its_own_exit_code() {
    let passed = check("Ran 3 tests in 0.001s\n\nOK\n", "", 0);
    assert_eq!(passed.exit_code, 0);
    assert_eq!(passed.stdout, "Ran 3 tests in 0.001s\n\nOK\n");
    let failed = check("", "FAILED (failures=2)\n", 1);
    assert_eq!(failed.exit_code, 1);
    assert_eq!(failed.stderr, "FAILED (failures=2)\n");
    let reserved = check("", "", DID_NOT_LOAD);
    assert_eq!(reserved.exit_code, 1);
}

// ---------------------------------------------------------------------------
// Clause 2: a built-in agent's bound fits two whole tries
// ---------------------------------------------------------------------------

/// A runtime whose every try takes a second to start and then works longer
/// than any clock in the test.
#[derive(Default)]
struct SlowRuntime {
    spawns: Mutex<u32>,
}

#[async_trait]
impl AgentRuntime for SlowRuntime {
    async fn spawn(&self, _config: RuntimeConfig) -> Result<InstanceId, RuntimeError> {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let mut spawns = self.spawns.lock().await;
        *spawns += 1;
        Ok(InstanceId::new(format!("instance-{spawns}")))
    }

    async fn execute(
        &self,
        _id: &InstanceId,
        _input: TaskInput,
    ) -> Result<TaskOutput, RuntimeError> {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok(TaskOutput {
            result: serde_json::Value::String("late".to_string()),
            logs: vec![],
            tool_calls: vec![],
            exit_code: 0,
            trajectory: vec![],
        })
    }

    async fn terminate(&self, _id: &InstanceId) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn status(&self, id: &InstanceId) -> Result<InstanceStatus, RuntimeError> {
        Ok(InstanceStatus {
            id: id.clone(),
            state: "running".to_string(),
            uptime_seconds: 0,
            memory_usage_mb: 0,
            cpu_usage_percent: 0.0,
        })
    }
}

/// Records which iterations started and which were recorded exhausted.
#[derive(Default)]
struct RecordingObserver {
    starts: Mutex<Vec<u8>>,
    exhausted: Mutex<Vec<(u8, ExhaustedBy)>>,
    failures: Mutex<Vec<String>>,
}

#[async_trait]
impl SupervisorObserver for RecordingObserver {
    async fn on_iteration_start(&self, iteration: u8, _prompt: &str) {
        self.starts.lock().await.push(iteration);
    }
    async fn on_console_output(&self, _iteration: u8, _stream: &str, _content: &str) {}
    async fn on_iteration_complete(&self, _iteration: u8, _result: &str, _exit_code: i64) {}
    async fn on_iteration_fail(&self, _iteration: u8, error: &str) {
        self.failures.lock().await.push(error.to_string());
    }
    async fn on_instance_spawned(&self, _iteration: u8, _instance_id: &InstanceId) {}
    async fn on_instance_terminated(&self, _iteration: u8, _instance_id: &InstanceId) {}
    async fn on_validation_complete(
        &self,
        _iteration: u8,
        _results: &ValidationResults,
        _passed: bool,
    ) {
    }
    async fn on_iteration_exhausted(&self, iteration: u8, by: ExhaustedBy) {
        self.exhausted.lock().await.push((iteration, by));
    }
}

/// An iterative agent with a 10 s iteration timeout and a 5 s judge, under
/// the bound [`two_tries_bound`] gives it.
fn ten_second_tries_judged_in_five() -> RuntimeConfig {
    let execution = ExecutionStrategy {
        mode: ExecutionMode::Iterative,
        max_retries: 2,
        iteration_timeout: Some("10s".to_string()),
        validation: Some(vec![ValidatorSpec::Semantic {
            judge_agent: "code-quality-judge".to_string(),
            criteria: "The answer does what was asked.".to_string(),
            min_score: 0.7,
            min_confidence: 0.0,
            timeout_seconds: 5,
        }]),
        ..Default::default()
    };
    let bound = two_tries_bound(&execution).as_secs();
    RuntimeConfig {
        language: "python".to_string(),
        version: "3.12".to_string(),
        isolation: "process".to_string(),
        env: HashMap::new(),
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        container_uid: 1000,
        container_gid: 1000,
        resources: ResourceLimits {
            cpu_millis: None,
            memory_bytes: None,
            disk_bytes: None,
            timeout_seconds: Some(bound),
        },
        execution,
        volumes: Vec::new(),
        keep_container_on_failure: false,
        image: "python:3.12".to_string(),
        bootstrap_path: None,
        execution_id: ExecutionId::new(),
        workflow_execution_id: None,
        program_files: Vec::new(),
        program_input: None,
    }
}

/// Run `e7f2c2aa`'s CODE, scaled down: try 1 starts after a second, runs to
/// its 10 s clock and is recorded exhausted, and the supervisor waits its
/// 15 s; under the bound of 2 x (10 + 5) + 15 + 30 = 75 s, try 2 then starts.
/// Under the bound before (2 x 10 + 15 = 35 s), 9 s were left and try 2 was
/// refused, as the coder's second try was refused with 899 s left under 1815.
#[tokio::test(start_paused = true)]
async fn a_clock_ended_try_leaves_room_for_the_second() {
    let runtime = Arc::new(SlowRuntime::default());
    let observer = Arc::new(RecordingObserver::default());
    let config = ten_second_tries_judged_in_five();
    let bound = config.resources.timeout_seconds;

    let result = Supervisor::new(runtime.clone())
        .run_loop(
            config,
            ExecutionInput {
                intent: Some("Change the implementation".to_string()),
                input: serde_json::json!({}),
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
                attachments: Vec::new(),
            },
            2,
            observer.clone(),
            CancellationToken::new(),
            None,
        )
        .await;

    let mut complaints = Vec::new();
    if bound != Some(75) {
        complaints.push(format!("the bound is {bound:?} s, not 75"));
    }
    let starts = observer.starts.lock().await.clone();
    if starts != vec![1, 2] {
        complaints.push(format!(
            "iterations started: {starts:?}, expected [1, 2]; the execution ended {result:?}"
        ));
    }
    let exhausted = observer.exhausted.lock().await.clone();
    if exhausted.first() != Some(&(1, ExhaustedBy::Clock)) {
        complaints.push(format!(
            "recorded exhausted: {exhausted:?}, expected try 1 by its clock"
        ));
    }
    let spawns = *runtime.spawns.lock().await;
    if spawns != 2 {
        complaints.push(format!("{spawns} tries were spawned, expected 2"));
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// The bound of every Forge agent, computed from its own manifest: AUDIT's
/// three judges wait for each agent as long as that agent's bound.
#[test]
fn audits_timeouts_follow_its_agents_bounds() {
    let workflow = forge();
    let Some(StateKind::ParallelAgents { agents, .. }) = state(&workflow, "AUDIT").map(|s| &s.kind)
    else {
        panic!("AUDIT is a ParallelAgents state");
    };
    let mut complaints = Vec::new();
    for member in agents {
        let yaml = std::fs::read_to_string(format!(
            "{}/../../cli/templates/agents/{}.yaml",
            env!("CARGO_MANIFEST_DIR"),
            member.agent
        ))
        .unwrap_or_else(|e| panic!("{}'s manifest: {e}", member.agent));
        let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let execution: ExecutionStrategy =
            serde_yaml::from_value(doc["spec"]["execution"].clone()).unwrap();
        let bound = two_tries_bound(&execution).as_secs();
        if member.timeout_seconds != bound {
            complaints.push(format!(
                "AUDIT waits {} s for {}, whose bound is {bound} s",
                member.timeout_seconds, member.agent
            ));
        }
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

// ---------------------------------------------------------------------------
// Clause 5: the run's last line states the cause
// ---------------------------------------------------------------------------

/// FAILED's `sentence`, rendered over `blackboard` as the workflow worker
/// renders an Action state's arguments (Handlebars, a missing field empty).
fn failed_sentence(blackboard: serde_json::Value) -> String {
    let workflow = forge();
    let Some(StateKind::Action { args, .. }) = state(&workflow, "FAILED").map(|s| &s.kind) else {
        panic!("FAILED is an Action state");
    };
    let template = args.get("sentence").expect("FAILED writes a sentence");
    handlebars::Handlebars::new()
        .render_template(template, &blackboard)
        .unwrap_or_else(|e| panic!("FAILED's sentence does not render: {e}"))
}

/// Run `e7f2c2aa` failed in CODE, and its blackboard's FAILED sentence was
/// "": the sentence was built from DIFF and COMMIT only. A failed state with
/// an `on_failure` route leaves `{status: "failed", error}` on the
/// blackboard; FAILED's sentence now carries that error when DIFF and
/// COMMIT carry none.
#[test]
fn a_code_failure_is_the_failed_states_sentence() {
    let error = "Agent execution failed: Failed to execute task: Execution ended: 899 s left \
                 under the 1815 s bound, under the 900 s a try needs";
    let sentence = failed_sentence(serde_json::json!({
        "ANALYZE": {"status": "completed", "output": "the requirements"},
        "ARCHITECT": {"status": "completed", "output": "the design"},
        "TEST": {"status": "completed", "output": "Files written:\ntest/test_readme_section.py"},
        "CHECK_TESTS": {"status": "success", "exit_code": 1, "output": {"exit_code": 1, "stdout": "", "stderr": "FAILED (failures=1)"}},
        "CODE": {"status": "failed", "error": error},
        "repository": {"branch": "aegis/e7f2c2aa"},
    }));
    assert_eq!(sentence, error);
}

/// A guard: a failed DIFF or COMMIT keeps its own sentence, once.
#[test]
fn a_repository_steps_failure_keeps_its_own_sentence() {
    for step in ["DIFF", "COMMIT"] {
        let refusal = "nothing to commit: working tree is clean";
        let sentence = failed_sentence(serde_json::json!({
            "CODE": {"status": "completed", "output": "done"},
            step: {"status": "failed", "error": refusal, "output": {"sentence": refusal}},
        }));
        assert_eq!(sentence, refusal, "{step}'s failure");
    }
}

// The outputs below were printed by Python 3.12's unittest and pytest 9.0.2,
// the first two on the tester's file of run `e7f2c2aa` as TEST answered it.

const UNITTEST_SYNTAX_STDERR: &str = r#"test_a (test_ok.T.test_a) ... ok
test_readme_section (unittest.loader._FailedTest.test_readme_section) ... ERROR

======================================================================
ERROR: test_readme_section (unittest.loader._FailedTest.test_readme_section)
----------------------------------------------------------------------
ImportError: Failed to import test module: test_readme_section
Traceback (most recent call last):
  File "/usr/lib/python3.12/unittest/loader.py", line 394, in _find_test_path
    module = self._get_module_from_name(name)
             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
  File "/usr/lib/python3.12/unittest/loader.py", line 337, in _get_module_from_name
    __import__(name)
  File "@REPO@/test/test_readme_section.py", line 52
    'aegis workflow status <execution-id> --output json | jq \\'.status\\'',
                                                                        ^
SyntaxError: unexpected character after line continuation character


----------------------------------------------------------------------
Ran 2 tests in 0.000s

FAILED (errors=1)
"#;

const PYTEST_SYNTAX_STDOUT: &str = r#"
==================================== ERRORS ====================================
_________________ ERROR collecting test/test_readme_section.py _________________
/usr/local/lib/python3.12/dist-packages/_pytest/python.py:507: in importtestmodule
    mod = import_path(
/usr/local/lib/python3.12/dist-packages/_pytest/pathlib.py:587: in import_path
    importlib.import_module(module_name)
/usr/lib/python3.12/importlib/__init__.py:90: in import_module
    return _bootstrap._gcd_import(name[level:], package, level)
           ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
<frozen importlib._bootstrap>:1387: in _gcd_import
    ???
<frozen importlib._bootstrap>:1360: in _find_and_load
    ???
<frozen importlib._bootstrap>:1331: in _find_and_load_unlocked
    ???
<frozen importlib._bootstrap>:935: in _load_unlocked
    ???
/usr/local/lib/python3.12/dist-packages/_pytest/assertion/rewrite.py:188: in exec_module
    source_stat, co = _rewrite_test(fn, self.config)
                      ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
/usr/local/lib/python3.12/dist-packages/_pytest/assertion/rewrite.py:357: in _rewrite_test
    tree = ast.parse(source, filename=strfn)
           ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
/usr/lib/python3.12/ast.py:52: in parse
    return compile(source, filename, mode, flags,
E     File "@REPO@/test/test_readme_section.py", line 52
E       'aegis workflow status <execution-id> --output json | jq \\'.status\\'',
E                                                                           ^
E   SyntaxError: unexpected character after line continuation character
=========================== short test summary info ============================
ERROR test/test_readme_section.py
!!!!!!!!!!!!!!!!!!!! Interrupted: 1 error during collection !!!!!!!!!!!!!!!!!!!!
1 error in 0.15s
"#;

const UNITTEST_MISSING_MODULE_STDERR: &str = r#"test_greeting (unittest.loader._FailedTest.test_greeting) ... ERROR

======================================================================
ERROR: test_greeting (unittest.loader._FailedTest.test_greeting)
----------------------------------------------------------------------
ImportError: Failed to import test module: test_greeting
Traceback (most recent call last):
  File "/usr/lib/python3.12/unittest/loader.py", line 394, in _find_test_path
    module = self._get_module_from_name(name)
             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
  File "/usr/lib/python3.12/unittest/loader.py", line 337, in _get_module_from_name
    __import__(name)
  File "@REPO@/test/test_greeting.py", line 2, in <module>
    from greeting import hello
ModuleNotFoundError: No module named 'greeting'


----------------------------------------------------------------------
Ran 1 test in 0.000s

FAILED (errors=1)
"#;

const PYTEST_MISSING_MODULE_STDOUT: &str = r#"
==================================== ERRORS ====================================
____________________ ERROR collecting test/test_greeting.py ____________________
ImportError while importing test module '@REPO@/test/test_greeting.py'.
Hint: make sure your test modules/packages have valid Python names.
Traceback:
/usr/lib/python3.12/importlib/__init__.py:90: in import_module
    return _bootstrap._gcd_import(name[level:], package, level)
           ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
test/test_greeting.py:2: in <module>
    from greeting import hello
E   ModuleNotFoundError: No module named 'greeting'
=========================== short test summary info ============================
ERROR test/test_greeting.py
!!!!!!!!!!!!!!!!!!!! Interrupted: 1 error during collection !!!!!!!!!!!!!!!!!!!!
1 error in 0.07s
"#;
