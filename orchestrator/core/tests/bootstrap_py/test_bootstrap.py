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
import socketserver
import subprocess
import sys
import threading
import time
import unittest

# Loading the bootstrap must not leave a __pycache__ beside it in assets/,
# which the images copy whole.
sys.dont_write_bytecode = True

BOOTSTRAP = pathlib.Path(__file__).resolve().parents[4] / "assets" / "bootstrap.py"


def load_bootstrap():
    spec = importlib.util.spec_from_file_location("aegis_bootstrap", BOOTSTRAP)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class SilentServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    """An orchestrator stand-in that reads each request and never answers."""

    daemon_threads = True

    def __init__(self):
        self.requests_seen = 0
        self.release = threading.Event()
        server = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                length = int(self.headers.get("Content-Length", 0))
                self.rfile.read(length)
                server.requests_seen += 1
                server.release.wait(10)

            def log_message(self, *args):
                pass

        super().__init__(("127.0.0.1", 0), Handler)

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server_address[1]}"


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


class PostJsonTimeoutTests(unittest.TestCase):
    """T5: a request that was delivered and not answered in time is not re-sent."""

    def setUp(self):
        self.bootstrap = load_bootstrap()
        self.servers = [SilentServer(), SilentServer()]
        for server in self.servers:
            threading.Thread(target=server.serve_forever, daemon=True).start()
        urls = [server.url for server in self.servers]
        self.bootstrap._candidate_urls = lambda: list(urls)

    def tearDown(self):
        for server in self.servers:
            server.release.set()
            server.shutdown()
            server.server_close()

    def test_read_timeout_is_not_retried_on_other_candidates(self):
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            with self.assertRaises(SystemExit) as raised:
                self.bootstrap.post_json({"type": "generate"}, timeout=1)

        self.assertEqual(raised.exception.code, 1)
        self.assertEqual(
            [server.requests_seen for server in self.servers],
            [1, 0],
            "the generate request must be sent once, to the first candidate",
        )
        first = f"{self.servers[0].url}/v1/dispatch-gateway"
        self.assertIn(
            f"Error: no answer from {first} within 1 s (AEGIS_ITERATION_TIMEOUT_SECONDS)",
            stderr.getvalue(),
        )

    def test_unreachable_candidate_falls_through_to_the_next(self):
        # A port nothing listens on: the connection is refused before any
        # request is sent, so the next candidate is tried.
        with socketserver.TCPServer(("127.0.0.1", 0), None) as probe:
            dead = f"http://127.0.0.1:{probe.server_address[1]}"
        live = self.servers[0].url
        self.bootstrap._candidate_urls = lambda: [dead, live]

        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            with self.assertRaises(SystemExit):
                self.bootstrap.post_json({"type": "generate"}, timeout=1)

        self.assertEqual(self.servers[0].requests_seen, 1, stderr.getvalue())


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


class IterationBoundTests(unittest.TestCase):
    """The bootstrap's wait on the dispatch gateway spans the whole inner tool
    loop of an iteration (many model calls and tool rounds), so it is bounded
    by the iteration's own bound, AEGIS_ITERATION_TIMEOUT_SECONDS, and not by
    a timeout meant for one model call (f6209dbc, 2026-10-03: three working
    iterations cut at 300 s, below the supervisor's 600 s)."""

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

    def test_answer_after_llm_timeout_and_before_iteration_bound_is_received(self):
        # The loop's answer comes 2 s after the generate POST: past the 1 s a
        # model call is given, inside the iteration's 6 s.
        server = self.start([(2, {"type": "final", "content": "the answer"})])
        done = self.run_main(
            server,
            {"AEGIS_LLM_TIMEOUT_SECONDS": "1", "AEGIS_ITERATION_TIMEOUT_SECONDS": "6"},
        )
        self.assertEqual(done.returncode, 0, done.stderr.decode())
        self.assertEqual(done.stdout.decode().strip(), "the answer")
        self.assertNotIn(b"no answer from", done.stderr)

    def test_repeat_post_after_a_dispatch_is_bounded_by_the_iteration(self):
        # The first answer is a dispatch; the repeat POST carrying its result
        # is never answered, so the bootstrap gives up at the iteration's bound
        # and says so, rather than waiting without end.
        server = self.start(
            [
                (0, {"type": "dispatch", "action": "exec", "dispatch_id": "d",
                     "command": "true", "cwd": "/"}),
                (0, None),
            ]
        )
        done = self.run_main(server, {"AEGIS_ITERATION_TIMEOUT_SECONDS": "2"}, wall=15)
        self.assertEqual(done.returncode, 1, done.stderr.decode())
        self.assertEqual([r["type"] for r in server.requests], ["generate", "dispatch_result"])
        self.assertIn(
            f"Error: no answer from {server.url}/v1/dispatch-gateway within 2 s "
            "(AEGIS_ITERATION_TIMEOUT_SECONDS)",
            done.stderr.decode(),
        )


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


if __name__ == "__main__":
    os.environ.setdefault("AEGIS_MODEL_ALIAS", "default")
    unittest.main()
