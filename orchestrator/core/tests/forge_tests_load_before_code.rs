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

use aegis_orchestrator_core::domain::workflow::{StateKind, Workflow, WorkflowState};
use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;
use std::collections::HashMap;
use std::process::Command;

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
