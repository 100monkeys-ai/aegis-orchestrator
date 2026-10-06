#!/usr/bin/env python3
"""AEGIS bootstrap.py — implements the Aegis Dispatch Protocol (ADR-040).

This script is injected into agent containers by the orchestrator (ADR-043).
It runs the 100monkeys inner loop by communicating with the orchestrator via
the bidirectional /v1/dispatch-gateway channel:

  1. POST {type:"generate", ...} → orchestrator starts inner loop with LLM
  2. Orchestrator may reply with {type:"dispatch", action:"exec", ...} to run
     a command inside this container (Path 3 tool routing, ADR-040).
  3. bootstrap.py executes the command via subprocess.run(), re-POSTs the result
     as {type:"dispatch_result", ...}, and waits for the next reply.
  4. When orchestrator replies {type:"final", ...} bootstrap prints content and exits.

DESIGN CONSTRAINTS (DO NOT VIOLATE):
  - stdlib-only: no third-party imports (Ultra-Thin Client, ADR-040 §Design Principles)
  - All policy enforcement is server-side; bootstrap.py is a trusted executor
  - Add complexity to the orchestrator, not here
"""

import base64
import json
import os
import shlex
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request

# ---------------------------------------------------------------------------
# SEAL configuration
# ---------------------------------------------------------------------------

SEAL_ENABLED = os.environ.get("AEGIS_SEAL_ENABLED", "").lower() in ("true", "1", "yes")
SEAL_GATEWAY_URL = os.environ.get(
    "AEGIS_SEAL_GATEWAY_URL", "http://host.docker.internal:8090"
)

if SEAL_ENABLED:
    try:
        from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
        from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat
    except ImportError:
        print(
            "Error: AEGIS_SEAL_ENABLED=1 but 'cryptography' package is not installed.",
            file=sys.stderr,
        )
        sys.exit(1)


class SealPolicyViolation(Exception):
    """Raised when the SEAL gateway rejects a tool call due to a policy violation."""


class SealClient:
    """SEAL attestation client for agent bootstrap. Performs Ed25519 key generation,
    attestation handshake with the orchestrator, and SEAL-wrapped tool invocation."""

    def __init__(
        self,
        orchestrator_url: str,
        seal_gateway_url: str,
        agent_id: str,
        execution_id: str,
        container_id: str,
        security_context: str,
    ):
        self.orchestrator_url = orchestrator_url.rstrip("/")
        self.seal_gateway_url = seal_gateway_url.rstrip("/")
        self.agent_id = agent_id
        self.execution_id = execution_id
        self.container_id = container_id
        self.security_context = security_context
        self._private_key = Ed25519PrivateKey.generate()
        self._public_key_b64 = base64.b64encode(
            self._private_key.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)
        ).decode("utf-8")
        self.security_token = None
        self.session_id = None

    def attest(self):
        """Perform SEAL attestation handshake and store the returned SecurityToken."""
        payload = json.dumps(
            {
                "public_key": self._public_key_b64,
                "container_id": self.container_id,
                "security_context": self.security_context,
                "agent_id": self.agent_id,
                "execution_id": self.execution_id,
            }
        ).encode("utf-8")

        req = urllib.request.Request(
            f"{self.orchestrator_url}/v1/seal/attest",
            data=payload,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        with urllib.request.urlopen(req, timeout=10) as resp:
            data = json.loads(resp.read())

        self.security_token = data["security_token"]
        self.session_id = data.get("session_id")

    def call_tool(self, tool_name: str, arguments: dict) -> dict:
        """Invoke a tool through the SEAL gateway with a signed envelope."""
        if self.security_token is None:
            raise RuntimeError("Must call attest() before call_tool()")

        mcp_payload = {
            "jsonrpc": "2.0",
            "id": f"req-{int(time.time() * 1000)}",
            "method": "tools/call",
            "params": {"name": tool_name, "arguments": arguments},
        }
        ts = int(time.time())

        canonical = json.dumps(
            {
                "payload": mcp_payload,
                "security_token": self.security_token,
                "timestamp": ts,
            },
            sort_keys=True,
        ).encode("utf-8")

        signature_bytes = self._private_key.sign(canonical)
        signature_b64 = base64.b64encode(signature_bytes).decode("utf-8")

        envelope = {
            "protocol": "seal/v1",
            "security_token": self.security_token,
            "signature": signature_b64,
            "payload": mcp_payload,
            "timestamp": ts,
        }

        req_data = json.dumps(envelope).encode("utf-8")
        req = urllib.request.Request(
            f"{self.seal_gateway_url}/v1/seal/invoke",
            data=req_data,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        with urllib.request.urlopen(req, timeout=30) as resp:
            result = json.loads(resp.read())

        if result.get("status") == "policy_violation":
            raise SealPolicyViolation(result.get("error", "policy violation"))

        return result.get("payload", {}).get("result", result)


# ---------------------------------------------------------------------------
# Debug helpers
# ---------------------------------------------------------------------------

DEBUG = os.environ.get("AEGIS_BOOTSTRAP_DEBUG", "").lower() in ("true", "1", "yes")


def debug_print(*args, **kwargs):
    """Print to stderr only when AEGIS_BOOTSTRAP_DEBUG is enabled."""
    if DEBUG:
        print("[BOOTSTRAP DEBUG]", *args, file=sys.stderr, **kwargs)


# ---------------------------------------------------------------------------
# HTTP transport
# ---------------------------------------------------------------------------


def _candidate_urls() -> list:
    """Return deduplicated orchestrator base URLs to try, in priority order."""
    candidates = []
    env_url = os.environ.get("AEGIS_ORCHESTRATOR_URL", "").rstrip("/")
    if env_url:
        candidates.append(env_url)
    candidates.append("http://host.docker.internal:8088")
    candidates.append("http://host.containers.internal:8088")
    seen = set()
    result = []
    for u in candidates:
        if u not in seen:
            seen.add(u)
            result.append(u)
    return result


def post_json(payload: dict) -> dict:
    """POST JSON to /v1/dispatch-gateway, trying all candidate URLs in order.

    The request carries no timeout (ADR-040, "bootstrap.py — Dispatch Loop":
    no timeout is required for long-running dispatch loops): the orchestrator
    runs the whole inner tool loop, many model calls and tool rounds, inside
    one request. The orchestrator bounds the wait, not this script: the inner
    loop bounds each model call (llm_timeout_seconds), and the supervisor
    bounds the iteration and the execution by terminating this container.
    An HTTP error answer exits with status 1. A failure to reach or hear from
    a candidate (refused, unresolvable, unreachable, reset) tries the next
    one. Exits with status 1 if all candidates fail.
    """
    data = json.dumps(payload).encode("utf-8")
    errors = []
    for base_url in _candidate_urls():
        url = f"{base_url}/v1/dispatch-gateway"
        debug_print(f"POST → {url} ({len(data)} bytes, type={payload.get('type')})")
        req = urllib.request.Request(
            url,
            data=data,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(req, timeout=None) as resp:
                body = resp.read().decode("utf-8")
                debug_print(f"← {resp.status} ({len(body)} bytes)")
                return json.loads(body)
        except urllib.error.HTTPError as e:
            # Server IS reachable but returned an error — do NOT fall through
            # to the next candidate URL.  Only connection-level failures
            # (URLError / OSError) should trigger URL fallback.
            body = e.read().decode("utf-8")
            debug_print(f"HTTP error: {base_url}: HTTP {e.code} {e.reason} — {body}")
            print(
                f"Error: Orchestrator returned HTTP {e.code}: {body}",
                file=sys.stderr,
            )
            sys.exit(1)
        except urllib.error.URLError as e:
            # Raised while connecting or sending: the request did not reach
            # this candidate, so the next one is tried.
            err = f"{base_url}: {e}"
            errors.append(err)
            debug_print(f"Connection error: {err}")
        except Exception as e:
            err = f"{base_url}: {e}"
            errors.append(err)
            debug_print(f"Connection error: {err}")

    print(
        "Error: Failed to reach orchestrator.\n"
        f"Tried: {_candidate_urls()}\n"
        "Errors:\n" + "\n".join(errors),
        file=sys.stderr,
    )
    sys.exit(1)


# ---------------------------------------------------------------------------
# Dispatch execution — Path 3 (ADR-040)
# ---------------------------------------------------------------------------


def _end_command(proc) -> tuple:
    """End a timed-out command and everything in its session; return the
    stdout and stderr it wrote until then."""
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        pass
    try:
        return proc.communicate(timeout=5)
    except subprocess.TimeoutExpired as exc:
        # A process that left the session still holds the pipes: keep what
        # was read and stop reading.
        return exc.stdout or b"", exc.stderr or b""


def _keep_head_and_tail(data: bytes, budget: int, label: str, max_bytes: int) -> str:
    """`data` whole when it fits `budget`, else its first and last bytes with
    a line stating what was omitted, cut on character boundaries."""
    if len(data) <= budget:
        return data.decode("utf-8", errors="replace")
    head = budget // 2
    while head > 0 and (data[head] & 0xC0) == 0x80:
        head -= 1
    tail_start = len(data) - (budget - budget // 2)
    while tail_start < len(data) and (data[tail_start] & 0xC0) == 0x80:
        tail_start += 1
    tail = len(data) - tail_start
    omitted = len(data) - head - tail
    marker = (
        f"\n[AEGIS] max_output_bytes ({max_bytes}) cut this {label}: it was {len(data)} bytes; "
        f"the first {head} and the last {tail} are kept and the {omitted} between them "
        "are omitted here.\n"
    )
    return (
        data[:head].decode("utf-8", errors="replace")
        + marker
        + data[tail_start:].decode("utf-8", errors="replace")
    )


def cap_output(stdout_b: bytes, stderr_b: bytes, max_bytes: int) -> tuple:
    """Cap stdout and stderr together at `max_bytes`, keeping the head and
    the tail of each stream that does not fit and stating its true size.
    Returns (stdout, stderr, truncated)."""
    if len(stdout_b) + len(stderr_b) <= max_bytes:
        return (
            stdout_b.decode("utf-8", errors="replace"),
            stderr_b.decode("utf-8", errors="replace"),
            False,
        )
    stderr_budget = min(len(stderr_b), max_bytes // 2)
    stdout_budget = max_bytes - stderr_budget
    if len(stdout_b) < stdout_budget:
        stdout_budget = len(stdout_b)
        stderr_budget = max_bytes - stdout_budget
    return (
        _keep_head_and_tail(stdout_b, stdout_budget, "stdout", max_bytes),
        _keep_head_and_tail(stderr_b, stderr_budget, "stderr", max_bytes),
        True,
    )


def run_dispatch(msg: dict, execution_id: str) -> dict:
    """Execute a dispatch action and return the dispatch_result payload.

    Phase 1 implements ``action: "exec"`` only.  Unknown actions are reported
    back gracefully with exit_code=-1 so the orchestrator can inject a tool
    error into the LLM conversation without crashing the loop.
    """
    action = msg.get("action")
    dispatch_id = msg["dispatch_id"]

    if action == "exec":
        # The command line (AEGIS ADR-040, Update of 2026-10-06, R1 and R1a):
        # "command" as the operator wrote it, run by the shell, followed by
        # each of "args" quoted for the shell, so an argument holding a space
        # or a quote reaches the program whole. With no args (absent or [])
        # "command" runs as the shell line it is.
        args = msg.get("args") or []
        command = msg["command"]
        if args:
            command = command + " " + " ".join(shlex.quote(a) for a in args)
        # Standard input (R2, R1b): the dispatch's "stdin" written whole and
        # closed; without it, an immediate end of input, never the
        # bootstrap's own standard input.
        stdin_text = msg.get("stdin")
        stdin_bytes = None if stdin_text is None else stdin_text.encode("utf-8")
        cwd = msg.get("cwd", "/workspace")
        timeout_secs = msg.get("timeout_secs", 60)
        max_bytes = msg.get("max_output_bytes", 1048576)  # the orchestrator's default

        # Inherit full container env then overlay orchestrator-supplied additions.
        # The orchestrator already scrubbed sensitive vars before building this message.
        env = os.environ.copy()
        env.update(msg.get("env_additions", {}))

        stdin_shown = "none" if stdin_bytes is None else f"{len(stdin_bytes)}B"
        debug_print(f"exec: {command!r} cwd={cwd!r} timeout={timeout_secs}s stdin={stdin_shown}")
        started_at = time.monotonic()
        try:
            # Its own session, so a timeout ends the command and everything
            # it started, not only the shell.
            proc = subprocess.Popen(
                command,
                shell=True,
                cwd=cwd,
                env=env,
                stdin=subprocess.DEVNULL if stdin_bytes is None else subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                start_new_session=True,
            )
        except OSError as e:
            # The command could not be started at all: a missing cwd or
            # shell (FileNotFoundError), a cwd that is not a directory or
            # not permitted. Reported as the shell would report it (127 for
            # a missing file, 126 otherwise) so the model sees a tool error
            # and the loop goes on.
            duration_ms = int((time.monotonic() - started_at) * 1000)
            exit_code = 127 if isinstance(e, FileNotFoundError) else 126
            debug_print(f"exec could not start: {e}")
            return {
                "type": "dispatch_result",
                "execution_id": execution_id,
                "dispatch_id": dispatch_id,
                "exit_code": exit_code,
                "stdout": "",
                "stderr": f"[AEGIS] cannot run command: {e}",
                "duration_ms": duration_ms,
                "truncated": False,
            }

        timed_out = False
        try:
            stdout_b, stderr_b = proc.communicate(input=stdin_bytes, timeout=timeout_secs)
        except subprocess.TimeoutExpired:
            timed_out = True
            stdout_b, stderr_b = _end_command(proc)
        duration_ms = int((time.monotonic() - started_at) * 1000)
        stdout_raw, stderr_raw, truncated = cap_output(stdout_b, stderr_b, max_bytes)
        if timed_out:
            debug_print(f"exec timed out after {timeout_secs}s")
            notice = (
                f"[AEGIS] Command timed out after {timeout_secs}s and was ended; "
                "its output until then is above."
            )
            stderr_raw = f"{stderr_raw}\n{notice}" if stderr_raw else notice
        debug_print(
            f"exec done: exit={-1 if timed_out else proc.returncode} "
            f"stdout={len(stdout_b)}B stderr={len(stderr_b)}B "
            f"duration={duration_ms}ms truncated={truncated}"
        )
        return {
            "type": "dispatch_result",
            "execution_id": execution_id,
            "dispatch_id": dispatch_id,
            "exit_code": -1 if timed_out else proc.returncode,
            "stdout": stdout_raw,
            "stderr": stderr_raw,
            "duration_ms": duration_ms,
            "truncated": truncated,
        }
    else:
        # Unknown action — report gracefully (ADR-040 §Dispatch DSL Action Vocabulary)
        debug_print(f"unknown dispatch action: {action!r}")
        return {
            "type": "dispatch_result",
            "execution_id": execution_id,
            "dispatch_id": dispatch_id,
            "exit_code": -1,
            "stdout": "",
            "stderr": f"unknown_action:{action}",
            "duration_ms": 0,
            "truncated": False,
        }


# ---------------------------------------------------------------------------
# Iteration history context builder
# ---------------------------------------------------------------------------


def _clean_str(s):
    """Unwrap values that were double-encoded by Rust's Value::to_string()."""
    if isinstance(s, str) and s.startswith('"') and s.endswith('"'):
        try:
            return json.loads(s)
        except json.JSONDecodeError:
            pass
    return s


def build_history_context(history_json: str) -> str:
    """Return a formatted history prefix from the AEGIS_ITERATION_HISTORY env var."""
    try:
        history = json.loads(history_json)
    except json.JSONDecodeError:
        return ""
    if not history:
        return ""
    ctx = "\n\n# Previous Attempts:\n"
    for item in history:
        ctx += f"\n## Iteration {item.get('iteration', '?')}:\n"
        if item.get("output"):
            ctx += f"Output:\n{_clean_str(item['output'])}\n"
        if item.get("error"):
            ctx += f"Error:\n{_clean_str(item['error'])}\n"
        # Prefer rich GradientResult feedback; fall back to validation_reason when
        # the validation pipeline itself errored rather than returning a score.
        if item.get("feedback"):
            ctx += f"Feedback:\n{_clean_str(item['feedback'])}\n"
        elif item.get("validation_reason"):
            ctx += f"Validation Failed:\n{_clean_str(item['validation_reason'])}\n"
    ctx += "\n# Current Attempt:\n"
    return ctx


# ---------------------------------------------------------------------------
# Config helpers
# ---------------------------------------------------------------------------


def read_prompt(argv, stdin) -> str:
    """The rendered prompt: argv[1] when given, else standard input.

    The orchestrator writes the fully rendered prompt to the exec's standard
    input and closes it: Linux refuses one exec argument over 128 KiB, and a
    judge's prompt carrying a worker's tool history is longer. The bytes are
    read whole and decoded as UTF-8 whatever the container's locale, nothing
    stripped, so the prompt arrives byte for byte.
    """
    if len(argv) > 1:
        return argv[1]
    return stdin.buffer.read().decode("utf-8")


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def main():
    # -- Config ---------------------------------------------------------------
    execution_id = os.environ.get("AEGIS_EXECUTION_ID")
    agent_id = os.environ.get("AEGIS_AGENT_ID", "")
    iteration_number = int(os.environ.get("AEGIS_ITERATION", "1"))

    # AEGIS_MODEL_ALIAS is injected by the orchestrator from spec.runtime.model.
    # It routes this execution to the correct provider alias (e.g. "judge",
    # "smart", "default"). The orchestrator MUST always inject this variable;
    # a missing value indicates a misconfiguration that must be fixed at source.
    model_alias = os.environ.get("AEGIS_MODEL_ALIAS")
    if model_alias is None:
        print(
            "Error: AEGIS_MODEL_ALIAS environment variable is not set. "
            "The orchestrator must inject this for every execution via spec.runtime.model.",
            file=sys.stderr,
        )
        sys.exit(1)

    debug_print(
        f"execution_id={execution_id} agent_id={agent_id} "
        f"iteration={iteration_number} model_alias={model_alias}"
    )

    # -- Prompt ---------------------------------------------------------------
    rendered_prompt = read_prompt(sys.argv, sys.stdin)

    if not rendered_prompt:
        print("Error: No prompt provided", file=sys.stderr)
        sys.exit(1)

    debug_print(f"Prompt received ({len(rendered_prompt)} chars)")

    # -- Iteration history context --------------------------------------------
    history_context = build_history_context(
        os.environ.get("AEGIS_ITERATION_HISTORY", "[]")
    )
    final_prompt = (
        history_context + rendered_prompt if history_context else rendered_prompt
    )

    # -- SEAL attestation (optional) ------------------------------------------
    seal_client = None
    if SEAL_ENABLED:
        orchestrator_url = (
            _candidate_urls()[0]
            if _candidate_urls()
            else "http://host.docker.internal:8088"
        )
        seal_client = SealClient(
            orchestrator_url=orchestrator_url,
            seal_gateway_url=SEAL_GATEWAY_URL,
            agent_id=agent_id,
            execution_id=execution_id or "",
            container_id=os.environ.get("HOSTNAME", ""),
            security_context=os.environ.get("AEGIS_SECURITY_CONTEXT", ""),
        )
        seal_client.attest()
        debug_print(f"SEAL attestation complete, session_id={seal_client.session_id}")

    # -- Dispatch loop (ADR-040) ----------------------------------------------
    # Send the initial generate request; the response may be a dispatch command
    # (type="dispatch") or the final LLM output (type="final").
    msg = post_json(
        {
            "type": "generate",
            "agent_id": agent_id,
            "execution_id": execution_id,
            "iteration_number": iteration_number,
            "model_alias": model_alias,
            "prompt": final_prompt,
            "messages": [],
        }
    )

    # Execute dispatch commands until the orchestrator issues type="final".
    while msg.get("type") == "dispatch":
        debug_print(
            f"dispatch: action={msg.get('action')!r} "
            f"dispatch_id={msg.get('dispatch_id')}"
        )
        result = run_dispatch(msg, execution_id)
        # Re-POST the result; the orchestrator will continue the LLM conversation
        # and may dispatch another command or issue the final response.
        msg = post_json(result)

    # type="final" — print the LLM response to stdout and exit cleanly.
    print(msg.get("content", ""))


if __name__ == "__main__":
    main()
