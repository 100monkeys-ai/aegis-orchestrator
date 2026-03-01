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
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

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
    seen = set()
    result = []
    for u in candidates:
        if u not in seen:
            seen.add(u)
            result.append(u)
    return result


def post_json(payload: dict, timeout: int = 0) -> dict:
    """POST JSON to /v1/dispatch-gateway, trying all candidate URLs in order.

    timeout=0 means no timeout (required when waiting for LLM responses or
    long-running dispatch results — see ADR-040 §bootstrap.py Dispatch Loop).
    Exits the process with status 1 if all candidates fail.
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
            with urllib.request.urlopen(req, timeout=timeout or None) as resp:
                body = resp.read().decode("utf-8")
                debug_print(f"← {resp.status} ({len(body)} bytes)")
                return json.loads(body)
        except urllib.error.HTTPError as e:
            body = e.read().decode("utf-8")
            err = f"{base_url}: HTTP {e.code} {e.reason} — {body}"
            errors.append(err)
            debug_print(f"HTTP error: {err}")
        except Exception as e:
            err = f"{base_url}: {e}"
            errors.append(err)
            debug_print(f"Error: {err}")

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

def run_dispatch(msg: dict, execution_id: str) -> dict:
    """Execute a dispatch action and return the dispatch_result payload.

    Phase 1 implements ``action: "exec"`` only.  Unknown actions are reported
    back gracefully with exit_code=-1 so the orchestrator can inject a tool
    error into the LLM conversation without crashing the loop.
    """
    action = msg.get("action")
    dispatch_id = msg["dispatch_id"]

    if action == "exec":
        # Build a shell command string. The orchestrator sends "command" as a
        # single string (e.g. "touch /workspace/hello.txt") and optional "args".
        parts = [msg["command"]] + msg.get("args", [])
        command = " ".join(parts)
        cwd = msg.get("cwd", "/workspace")
        timeout_secs = msg.get("timeout_secs", 60)
        max_bytes = msg.get("max_output_bytes", 524288)  # 512 KB default

        # Inherit full container env then overlay orchestrator-supplied additions.
        # The orchestrator already scrubbed sensitive vars before building this message.
        env = os.environ.copy()
        env.update(msg.get("env_additions", {}))

        debug_print(f"exec: {command!r} cwd={cwd!r} timeout={timeout_secs}s")
        started_at = time.monotonic()
        try:
            result = subprocess.run(
                command,
                shell=True,
                cwd=cwd,
                env=env,
                capture_output=True,
                timeout=timeout_secs,
            )
            duration_ms = int((time.monotonic() - started_at) * 1000)
            stdout_raw = result.stdout.decode("utf-8", errors="replace")
            stderr_raw = result.stderr.decode("utf-8", errors="replace")
            combined_size = len((stdout_raw + stderr_raw).encode("utf-8"))
            truncated = combined_size > max_bytes
            if truncated:
                # Keep the tail — most diagnostic for the LLM
                half = max_bytes // 2
                stdout_raw = stdout_raw[-half:]
                stderr_raw = stderr_raw[-half:]
            debug_print(
                f"exec done: exit={result.returncode} "
                f"stdout={len(stdout_raw)}B stderr={len(stderr_raw)}B "
                f"duration={duration_ms}ms truncated={truncated}"
            )
            return {
                "type": "dispatch_result",
                "execution_id": execution_id,
                "dispatch_id": dispatch_id,
                "exit_code": result.returncode,
                "stdout": stdout_raw,
                "stderr": stderr_raw,
                "duration_ms": duration_ms,
                "truncated": truncated,
            }
        except subprocess.TimeoutExpired:
            duration_ms = int((time.monotonic() - started_at) * 1000)
            debug_print(f"exec timed out after {timeout_secs}s")
            return {
                "type": "dispatch_result",
                "execution_id": execution_id,
                "dispatch_id": dispatch_id,
                "exit_code": -1,
                "stdout": "",
                "stderr": f"[AEGIS] Command timed out after {timeout_secs}s",
                "duration_ms": duration_ms,
                "truncated": False,
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

def _parse_timeout() -> int:
    """Parse AEGIS_LLM_TIMEOUT_SECONDS; return default 300 on invalid input."""
    raw = os.environ.get("AEGIS_LLM_TIMEOUT_SECONDS", "300")
    try:
        return int(raw)
    except ValueError:
        debug_print(
            f"Warning: Invalid AEGIS_LLM_TIMEOUT_SECONDS value {raw!r}, using default 300s"
        )
        return 300


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

def main():
    # -- Config ---------------------------------------------------------------
    execution_id = os.environ.get("AEGIS_EXECUTION_ID")
    agent_id = os.environ.get("AEGIS_AGENT_ID", "")
    iteration_number = int(os.environ.get("AEGIS_ITERATION", "1"))
    llm_timeout_seconds = _parse_timeout()

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
        f"iteration={iteration_number} model_alias={model_alias} "
        f"timeout={llm_timeout_seconds}s"
    )

    # -- Prompt ---------------------------------------------------------------
    # The orchestrator fully renders the prompt before passing it in (argv[1] or stdin).
    if len(sys.argv) > 1:
        rendered_prompt = sys.argv[1]
    else:
        rendered_prompt = sys.stdin.read().strip()

    if not rendered_prompt:
        print("Error: No prompt provided", file=sys.stderr)
        sys.exit(1)

    debug_print(f"Prompt received ({len(rendered_prompt)} chars)")

    # -- Iteration history context --------------------------------------------
    history_context = build_history_context(
        os.environ.get("AEGIS_ITERATION_HISTORY", "[]")
    )
    final_prompt = history_context + rendered_prompt if history_context else rendered_prompt

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
        },
        timeout=llm_timeout_seconds,
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
        msg = post_json(result, timeout=llm_timeout_seconds)

    # type="final" — print the LLM response to stdout and exit cleanly.
    print(msg.get("content", ""))


if __name__ == "__main__":
    main()
