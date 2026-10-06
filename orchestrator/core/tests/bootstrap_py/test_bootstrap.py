# Copyright (c) 2026 100monkeys.ai
# SPDX-License-Identifier: AGPL-3.0
"""Tests of assets/bootstrap.py, the in-container half of the Aegis Dispatch
Protocol (ADR-040). Stdlib only, as the bootstrap is. Run by
`tests/bootstrap_dispatch_tests.rs` under `cargo test`, or directly:

    python3 orchestrator/core/tests/bootstrap_py/test_bootstrap.py -v
"""

import contextlib
import http.server
import importlib.util
import io
import json
import os
import pathlib
import socket
import socketserver
import struct
import subprocess
import sys
import threading
import time
import unittest
import unittest.mock

# Loading the bootstrap must not leave a __pycache__ beside it in assets/,
# which the images copy whole.
sys.dont_write_bytecode = True

BOOTSTRAP = pathlib.Path(__file__).resolve().parents[4] / "assets" / "bootstrap.py"


def load_bootstrap():
    spec = importlib.util.spec_from_file_location("aegis_bootstrap", BOOTSTRAP)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class RunDispatchTests(unittest.TestCase):
    """T4: a command the container cannot start is a tool result, not a crash."""

    def setUp(self):
        self.bootstrap = load_bootstrap()

    def test_missing_cwd_is_reported_as_exit_127_naming_the_path(self):
        result = self.bootstrap.run_dispatch(
            {
                "action": "exec",
                "dispatch_id": "d",
                "command": "true",
                "cwd": "/nonexistent-aegis",
            },
            "e",
        )
        self.assertEqual(result["type"], "dispatch_result")
        self.assertEqual(result["dispatch_id"], "d")
        self.assertEqual(result["execution_id"], "e")
        self.assertEqual(result["exit_code"], 127)
        self.assertEqual(result["stdout"], "")
        self.assertIn("/nonexistent-aegis", result["stderr"])
        self.assertTrue(
            result["stderr"].startswith("[AEGIS] cannot run command: "), result["stderr"]
        )

    def test_cwd_that_is_not_a_directory_is_reported_as_exit_126(self):
        result = self.bootstrap.run_dispatch(
            {"action": "exec", "dispatch_id": "d", "command": "true", "cwd": str(BOOTSTRAP)},
            "e",
        )
        self.assertEqual(result["exit_code"], 126)
        self.assertIn(str(BOOTSTRAP), result["stderr"])

    def test_command_that_runs_reports_its_own_exit_code(self):
        result = self.bootstrap.run_dispatch(
            {"action": "exec", "dispatch_id": "d", "command": "exit 3", "cwd": "/"},
            "e",
        )
        self.assertEqual(result["exit_code"], 3)


class CommandTimeoutTests(unittest.TestCase):
    """AEGIS ADR-005 / ADR-040 (retry-knows-what-failed): a command that runs
    past its timeout ends, with everything it started, and the model is given
    its output until then with the command's own timeout result."""

    def setUp(self):
        self.bootstrap = load_bootstrap()

    def test_a_timed_out_command_returns_its_output_so_far(self):
        started = time.monotonic()
        result = self.bootstrap.run_dispatch(
            {
                "action": "exec",
                "dispatch_id": "d",
                "command": "echo tick 1; echo warn 1 >&2; sleep 30; echo never",
                "cwd": "/",
                "timeout_secs": 1,
            },
            "e",
        )
        self.assertLess(time.monotonic() - started, 10)
        self.assertEqual(result["exit_code"], -1)
        self.assertIn("tick 1", result["stdout"])
        self.assertNotIn("never", result["stdout"])
        self.assertIn("warn 1", result["stderr"])
        self.assertIn("[AEGIS] Command timed out after 1s", result["stderr"])

    def test_a_timed_out_command_ends_what_it_started(self):
        with contextlib.ExitStack() as stack:
            import tempfile

            workdir = stack.enter_context(tempfile.TemporaryDirectory())
            marker = os.path.join(workdir, "late")
            self.bootstrap.run_dispatch(
                {
                    "action": "exec",
                    "dispatch_id": "d",
                    "command": f"(sleep 3; touch {marker}) & sleep 30",
                    "cwd": "/",
                    "timeout_secs": 1,
                },
                "e",
            )
            time.sleep(4)
            self.assertFalse(
                os.path.exists(marker),
                "a child of the timed-out command kept running after its result",
            )


class OutputCapTests(unittest.TestCase):
    """The max_output_bytes cap keeps the head and the tail and states the
    output's true size in words (ADR-040's cap; AEGIS ADR-131 U19 keeps it as
    the output as produced)."""

    def setUp(self):
        self.bootstrap = load_bootstrap()

    def run_command(self, command, max_output_bytes):
        return self.bootstrap.run_dispatch(
            {
                "action": "exec",
                "dispatch_id": "d",
                "command": command,
                "cwd": "/",
                "max_output_bytes": max_output_bytes,
            },
            "e",
        )

    def test_output_over_the_cap_keeps_head_and_tail_and_states_its_true_size(self):
        result = self.run_command(
            "python3 -c \"import sys; sys.stdout.write('HEAD' + 'x' * 10000 + 'TAIL')\"",
            1000,
        )
        self.assertTrue(result["truncated"])
        out = result["stdout"]
        self.assertTrue(out.startswith("HEAD"), out[:80])
        self.assertTrue(out.endswith("TAIL"), out[-80:])
        self.assertIn("max_output_bytes (1000)", out)
        self.assertIn("it was 10008 bytes", out)
        kept = len(out.encode("utf-8")) - len(
            out[out.index("\n[AEGIS]") : out.index("\n", out.index("\n[AEGIS]") + 1) + 1].encode(
                "utf-8"
            )
        )
        self.assertLessEqual(kept, 1000)

    def test_output_under_the_cap_is_whole_and_unmarked(self):
        result = self.run_command("printf 'abc'", 1000)
        self.assertFalse(result["truncated"])
        self.assertEqual(result["stdout"], "abc")

    def test_the_cut_never_splits_a_character(self):
        result = self.run_command(
            "python3 -c \"import sys; sys.stdout.write('\\u00e9' * 5000)\"", 1001
        )
        self.assertTrue(result["truncated"])
        self.assertNotIn("�", result["stdout"])


class ResettingServer(socketserver.ThreadingMixIn, socketserver.TCPServer):
    """An orchestrator stand-in whose process goes away mid-request: it reads
    each request and resets the connection without answering."""

    daemon_threads = True
    allow_reuse_address = True

    def __init__(self):
        self.requests_seen = 0
        server = self

        class Handler(socketserver.StreamRequestHandler):
            def handle(self):
                length = 0
                while True:
                    line = self.rfile.readline()
                    if line in (b"\r\n", b"\n", b""):
                        break
                    name, _, value = line.decode("latin-1").partition(":")
                    if name.strip().lower() == "content-length":
                        length = int(value.strip())
                self.rfile.read(length)
                server.requests_seen += 1
                # SO_LINGER with a zero timeout: close() sends a reset.
                self.connection.setsockopt(
                    socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0)
                )
                self.connection.close()

            def finish(self):
                pass

        super().__init__(("127.0.0.1", 0), Handler)

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server_address[1]}"


class PostJsonCandidateTests(unittest.TestCase):
    """Which failures move the request on to the next candidate URL, and that a
    reset connection ends the bootstrap with status 1."""

    def setUp(self):
        self.bootstrap = load_bootstrap()

    def start(self, server):
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        return server

    def test_unreachable_candidate_falls_through_to_the_next(self):
        # A port nothing listens on: the connection is refused before any
        # request is sent, so the next candidate is tried.
        with socketserver.TCPServer(("127.0.0.1", 0), None) as probe:
            dead = f"http://127.0.0.1:{probe.server_address[1]}"
        live = self.start(ScriptedServer([(0, {"type": "final", "content": "ok"})]))
        self.bootstrap._candidate_urls = lambda: [dead, live.url]

        msg = self.bootstrap.post_json({"type": "generate"})

        self.assertEqual(msg, {"type": "final", "content": "ok"})
        self.assertEqual([r["type"] for r in live.requests], ["generate"])

    def test_reset_connection_exits_with_status_1(self):
        # The orchestrator's process is gone: the kernel resets the connection
        # the bootstrap is waiting on. With no clock of its own, this is what
        # ends the bootstrap when nothing will ever answer.
        server = self.start(ResettingServer())
        self.bootstrap._candidate_urls = lambda: [server.url]

        stderr = io.StringIO()
        started = time.monotonic()
        with contextlib.redirect_stderr(stderr):
            with self.assertRaises(SystemExit) as raised:
                self.bootstrap.post_json({"type": "generate"})

        self.assertEqual(raised.exception.code, 1)
        self.assertEqual(server.requests_seen, 1)
        self.assertLess(time.monotonic() - started, 5)
        self.assertIn("Error: Failed to reach orchestrator.", stderr.getvalue())
        self.assertIn(server.url, stderr.getvalue())


class ScriptedServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    """An orchestrator stand-in that answers the n-th POST after the n-th
    delay with the n-th body; a body of None is never answered."""

    daemon_threads = True

    def __init__(self, script):
        self.script = list(script)
        self.requests = []
        self.release = threading.Event()
        server = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                length = int(self.headers.get("Content-Length", 0))
                server.requests.append(json.loads(self.rfile.read(length)))
                delay, body = server.script[len(server.requests) - 1]
                if body is None:
                    server.release.wait(30)
                    return
                time.sleep(delay)
                data = json.dumps(body).encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, *args):
                pass

        super().__init__(("127.0.0.1", 0), Handler)

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server_address[1]}"


class UnboundedWaitTests(unittest.TestCase):
    """The bootstrap's wait on the dispatch gateway carries no timeout (AEGIS
    ADR-040, "bootstrap.py — Dispatch Loop": "timeout=0 → no timeout (required
    for long-running dispatch loops)"; Gap 040-9: the wait after a dispatch
    result "should be unbounded"). The orchestrator bounds one model call
    (llm_timeout_seconds), and its supervisor bounds the iteration and the
    execution by terminating the container."""

    def start(self, script):
        server = ScriptedServer(script)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        self.addCleanup(server.release.set)
        return server

    def run_main(self, server, env_overrides, wall=20):
        env = dict(os.environ)
        env.pop("AEGIS_LLM_TIMEOUT_SECONDS", None)
        env.pop("AEGIS_ITERATION_TIMEOUT_SECONDS", None)
        env.update(
            AEGIS_MODEL_ALIAS="default",
            AEGIS_EXECUTION_ID="e",
            AEGIS_AGENT_ID="a",
            AEGIS_ORCHESTRATOR_URL=server.url,
        )
        env.update(env_overrides)
        return subprocess.run(
            [sys.executable, "-B", str(BOOTSTRAP)],
            input=b"do the work",
            capture_output=True,
            env=env,
            timeout=wall,
        )

    def test_answer_after_llm_timeout_is_received(self):
        # The loop's answer comes 2 s after the generate POST, past the 1 s a
        # model call is given.
        server = self.start([(2, {"type": "final", "content": "the answer"})])
        done = self.run_main(server, {"AEGIS_LLM_TIMEOUT_SECONDS": "1"})
        self.assertEqual(done.returncode, 0, done.stderr.decode())
        self.assertEqual(done.stdout.decode().strip(), "the answer")

    def test_answers_after_any_earlier_bound_are_received_on_both_posts(self):
        # The generate POST is answered with a dispatch after 2 s, and the
        # dispatch_result re-POST with the final answer after 2 s more: each
        # past the 1 s that 265469e7's bootstrap would have read from
        # AEGIS_ITERATION_TIMEOUT_SECONDS and given up at.
        server = self.start(
            [
                (2, {"type": "dispatch", "action": "exec", "dispatch_id": "d",
                     "command": "true", "cwd": "/"}),
                (2, {"type": "final", "content": "the answer"}),
            ]
        )
        done = self.run_main(
            server,
            {"AEGIS_LLM_TIMEOUT_SECONDS": "1", "AEGIS_ITERATION_TIMEOUT_SECONDS": "1"},
        )
        self.assertEqual(done.returncode, 0, done.stderr.decode())
        self.assertEqual(done.stdout.decode().strip(), "the answer")
        self.assertEqual(
            [r["type"] for r in server.requests], ["generate", "dispatch_result"]
        )

    def test_both_posts_are_made_with_no_timeout(self):
        # In process: every urlopen the dispatch loop makes is given
        # timeout=None, whatever the environment says.
        server = self.start(
            [
                (0, {"type": "dispatch", "action": "exec", "dispatch_id": "d",
                     "command": "true", "cwd": "/"}),
                (0, {"type": "final", "content": "the answer"}),
            ]
        )
        bootstrap = load_bootstrap()
        bootstrap._candidate_urls = lambda: [server.url]
        timeouts = []
        real_urlopen = bootstrap.urllib.request.urlopen

        def recording_urlopen(req, *args, **kwargs):
            timeouts.append(kwargs["timeout"] if "timeout" in kwargs else args[1])
            return real_urlopen(req, *args, **kwargs)

        env = {
            "AEGIS_MODEL_ALIAS": "default",
            "AEGIS_EXECUTION_ID": "e",
            "AEGIS_AGENT_ID": "a",
            "AEGIS_LLM_TIMEOUT_SECONDS": "1",
            "AEGIS_ITERATION_TIMEOUT_SECONDS": "1",
        }
        stdout = io.StringIO()
        with unittest.mock.patch.dict(os.environ, env), unittest.mock.patch.object(
            bootstrap.urllib.request, "urlopen", recording_urlopen
        ), unittest.mock.patch.object(
            sys, "argv", ["bootstrap.py", "do the work"]
        ), contextlib.redirect_stdout(stdout):
            bootstrap.main()

        self.assertEqual(stdout.getvalue().strip(), "the answer")
        self.assertEqual(
            [r["type"] for r in server.requests], ["generate", "dispatch_result"]
        )
        self.assertEqual(timeouts, [None, None])


class ReadPromptTests(unittest.TestCase):
    """The orchestrator sends the prompt on the exec's standard input, because
    Linux refuses one exec argument over 128 KiB ("argument list too long",
    eval-rubric-judge on 2026-10-02). The prompt must arrive byte for byte."""

    # Quotes, shell metacharacters, newlines at both ends, a carriage return,
    # tabs and non-ASCII text, repeated past the 128 KiB argument limit.
    UNIT = " He said \"don't\" — 'quoted' $HOME `ls` \\n\n\ttab, naïve café 日本語 🙂\r\n"
    PROMPT = "\n  " + UNIT * 4000 + "\n\n"

    def run_bootstrap_reading(self, stdin_bytes):
        """Reads the prompt in a fresh interpreter whose standard input is a
        pipe and whose locale is C, as in a slim container image."""
        reader = (
            "import importlib.util, sys\n"
            f"spec = importlib.util.spec_from_file_location('b', {str(BOOTSTRAP)!r})\n"
            "b = importlib.util.module_from_spec(spec); spec.loader.exec_module(b)\n"
            "sys.stdout.buffer.write(b.read_prompt(sys.argv[:1], sys.stdin).encode('utf-8'))\n"
        )
        env = dict(os.environ, LC_ALL="C", LANG="C", AEGIS_MODEL_ALIAS="default")
        env.pop("PYTHONIOENCODING", None)
        return subprocess.run(
            [sys.executable, "-B", "-c", reader],
            input=stdin_bytes,
            capture_output=True,
            env=env,
            check=True,
        ).stdout

    def test_prompt_on_standard_input_arrives_byte_for_byte(self):
        sent = self.PROMPT.encode("utf-8")
        self.assertGreater(len(sent), 128 * 1024)
        received = self.run_bootstrap_reading(sent)
        self.assertEqual(len(received), len(sent))
        self.assertEqual(received, sent)

    def test_prompt_given_as_argument_is_still_read(self):
        bootstrap = load_bootstrap()
        self.assertEqual(
            bootstrap.read_prompt(["bootstrap", self.UNIT], io.StringIO("ignored")),
            self.UNIT,
        )



class ArgvAndStdinTests(unittest.TestCase):
    """AEGIS ADR-040, Update of 2026-10-06 (R1, R2, R1a, R1b): an `exec`
    dispatch keeps its arguments and may carry standard input. Executions
    6c6999ec, 71e702a7 and 93637ef3 of `unit-conversion-agent`: `sh -c 'echo
    hello'` answered a bare newline, `python3 -c "print('hello')"` exited 2,
    and a script reading standard input could not be fed."""

    # Echoes the `value` of the JSON on its standard input.
    CONVERT = "import json, sys\nprint(json.load(sys.stdin)['value'])\n"
    # Reports what its standard input held, or that it had to wait for it.
    READ_ALL = "import sys\nprint(repr(sys.stdin.read()))\n"
    # Prints its first argument.
    FIRST_ARG = "import sys\nprint(sys.argv[1])\n"

    def setUp(self):
        self.bootstrap = load_bootstrap()
        self.dir = pathlib.Path(self._tmp())
        (self.dir / "convert.py").write_text(self.CONVERT)
        (self.dir / "read_all.py").write_text(self.READ_ALL)
        (self.dir / "first_arg.py").write_text(self.FIRST_ARG)

    def _tmp(self):
        import tempfile

        tmp = tempfile.TemporaryDirectory(prefix="aegis-argv-")
        self.addCleanup(tmp.cleanup)
        return tmp.name

    def run_exec(self, **fields):
        msg = {"action": "exec", "dispatch_id": "d", "cwd": str(self.dir), "timeout_secs": 10}
        msg.update(fields)
        return self.bootstrap.run_dispatch(msg, "e")

    def test_an_argument_holding_a_space_reaches_the_command_whole(self):
        result = self.run_exec(command="sh", args=["-c", "echo hello"])
        self.assertEqual(
            result["stdout"],
            "hello\n",
            f"sh -c 'echo hello' did not answer hello: {result}",
        )
        self.assertEqual(result["exit_code"], 0, result)

    def test_an_argument_holding_quotes_and_parentheses_reaches_the_command_whole(self):
        result = self.run_exec(command="python3", args=["-c", "print('hello')"])
        self.assertEqual(
            result["stdout"],
            "hello\n",
            f"python3 -c \"print('hello')\" did not answer hello: {result}",
        )
        self.assertEqual(result["exit_code"], 0, result)

    def test_a_command_written_as_a_shell_line_takes_its_arguments_after_it(self):
        # R1a: the command string as the operator wrote it, each argument
        # quoted after it.
        result = self.run_exec(command="python3 first_arg.py", args=["43"])
        self.assertEqual(
            result["exit_code"], 0, f"python3 first_arg.py 43 did not run: {result}"
        )
        self.assertEqual(result["stdout"], "43\n", result)

    def test_stdin_is_written_to_the_command_and_closed(self):
        result = self.run_exec(command="python3", args=["convert.py"], stdin='{"value":43}')
        self.assertEqual(
            result["stdout"],
            "43\n",
            f"the script reading stdin was not given the piped JSON: {result}",
        )
        self.assertEqual(result["exit_code"], 0, result)

    def test_without_stdin_the_command_reads_an_immediate_end_of_input(self):
        # R1b: never the bootstrap's own standard input. The bootstrap here
        # runs with an open pipe on its standard input that is never written
        # or closed, as a container's would be if the prompt pipe stayed open:
        # a command inheriting it waits until its timeout.
        runner = (
            "import importlib.util, json, sys\n"
            "sys.dont_write_bytecode = True\n"
            f"spec = importlib.util.spec_from_file_location('b', {str(BOOTSTRAP)!r})\n"
            "b = importlib.util.module_from_spec(spec); spec.loader.exec_module(b)\n"
            "msg = json.loads(sys.argv[1])\n"
            "sys.stdout.write(json.dumps(b.run_dispatch(msg, 'e')))\n"
        )
        msg = {
            "action": "exec",
            "dispatch_id": "d",
            "command": "python3",
            "args": ["read_all.py"],
            "cwd": str(self.dir),
            "timeout_secs": 3,
        }
        proc = subprocess.Popen(
            [sys.executable, "-B", "-c", runner, json.dumps(msg)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            env=dict(os.environ, AEGIS_MODEL_ALIAS="default"),
        )
        # Not communicate(), which would close the pipe: it stays open and
        # unwritten until the bootstrap has answered.
        try:
            proc.wait(timeout=30)
            out = proc.stdout.read()
        finally:
            proc.stdin.close()
            proc.stdout.close()
        result = json.loads(out)
        self.assertEqual(
            result["stdout"],
            "''\n",
            f"a command given no stdin did not read an immediate end of input: {result}",
        )
        self.assertEqual(result["exit_code"], 0, result)

    def test_a_shell_line_with_empty_args_runs_as_today(self):
        for fields in ({}, {"args": []}):
            result = self.run_exec(command="echo a b | tr a-z A-Z", **fields)
            self.assertEqual(result["stdout"], "A B\n", f"{fields}: {result}")
            self.assertEqual(result["exit_code"], 0, result)


if __name__ == "__main__":
    os.environ.setdefault("AEGIS_MODEL_ALIAS", "default")
    unittest.main()
