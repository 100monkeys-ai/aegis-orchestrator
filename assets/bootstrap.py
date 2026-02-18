#!/usr/bin/env python3
import os
import sys
import json
import urllib.request
import urllib.error

# Debug mode flag - set via AEGIS_BOOTSTRAP_DEBUG environment variable
DEBUG = os.environ.get("AEGIS_BOOTSTRAP_DEBUG", "").lower() in ("true", "1", "yes")

def debug_print(*args, **kwargs):
    """Print debug messages only when DEBUG mode is enabled."""
    if DEBUG:
        print("[BOOTSTRAP DEBUG]", *args, file=sys.stderr, **kwargs)

def main():
    # 1. Configuration
    orchestrator_url = os.environ.get("AEGIS_ORCHESTRATOR_URL", "http://host.docker.internal:8000")
    execution_id = os.environ.get("AEGIS_EXECUTION_ID")
    iteration_env = os.environ.get("AEGIS_ITERATION")
    iteration_number = int(iteration_env) if iteration_env else None
    
    debug_print(f"Bootstrap starting - execution_id={execution_id}, iteration={iteration_number}")
    debug_print(f"Orchestrator URL: {orchestrator_url}")
    
    # Previous iteration history (JSON array of {iteration, output, error})
    iteration_history_json = os.environ.get("AEGIS_ITERATION_HISTORY", "[]")
    
    # 2. Read Fully-Rendered Prompt
    # NOTE: As of the prompt preparation refactor, the prompt is now FULLY RENDERED
    # by ExecutionService before being passed to the agent. We no longer do template
    # rendering here - we receive the complete LLM prompt as argv[1].
    if len(sys.argv) > 1:
        rendered_prompt = sys.argv[1]
    else:
        # Fallback to stdin if needed
        rendered_prompt = sys.stdin.read().strip()

    if not rendered_prompt:
        print("Error: No prompt provided", file=sys.stderr)
        sys.exit(1)
    
    debug_print(f"Received prompt ({len(rendered_prompt)} chars)")

    # 3. Enhance with Iteration History (if retry)
    # Build iteration history context if this is a retry
    history_context = ""
    try:
        iteration_history = json.loads(iteration_history_json)
        if iteration_history:
            history_context = "\n\n# Previous Attempts:\n"
            for item in iteration_history:
                iter_num = item.get("iteration", "?")
                history_context += f"\n## Iteration {iter_num}:\n"
                
                if "output" in item and item["output"]:
                    history_context += f"Output:\n{item['output']}\n"
                
                if "error" in item and item["error"]:
                    history_context += f"Error:\n{item['error']}\n"
                
                if "feedback" in item and item["feedback"]:
                    history_context += f"Feedback:\n{item['feedback']}\n"
            
            history_context += "\n# Current Attempt:\n"
    except json.JSONDecodeError:
        # If history is malformed, continue without it
        pass
    
    # Prepend history context to the rendered prompt if we're in a retry
    final_prompt = rendered_prompt
    if history_context:
        final_prompt = f"{history_context}{rendered_prompt}"

    # Prepend history context to the rendered prompt if we're in a retry
    final_prompt = rendered_prompt
    if history_context:
        final_prompt = f"{history_context}{rendered_prompt}"

    # 4. Call LLM Proxy
    payload = {
        "prompt": final_prompt,
        "execution_id": execution_id,
        "iteration_number": iteration_number,
        "model": "default"
    }
    
    debug_print(f"Preparing LLM request - prompt length: {len(final_prompt)} chars")
    
    payload_data = json.dumps(payload).encode('utf-8')
    headers = {'Content-Type': 'application/json'}
    
    # Simple list: User override -> Default
    urls = []
    if os.environ.get("AEGIS_ORCHESTRATOR_URL"):
        urls.append(os.environ["AEGIS_ORCHESTRATOR_URL"])
    urls.append("http://host.docker.internal:8000")
    
    # Deduplicate
    urls = list(dict.fromkeys(urls))
    
    success = False
    errors = []
    
    for base_url in urls:
        debug_print(f"Attempting to connect to Orchestrator at {base_url}...")
        url = f"{base_url.rstrip('/')}/v1/llm/generate"
        debug_print(f"Trying URL: {url}")
        try:
            req = urllib.request.Request(
                url,
                data=payload_data,
                headers=headers
            )
            debug_print(f"Sending request with {len(payload_data)} bytes")
            with urllib.request.urlopen(req, timeout=10) as response:
                if 200 <= response.status < 300:
                    body = response.read().decode('utf-8')
                    debug_print(f"Received response: {response.status}, {len(body)} bytes")
                    data = json.loads(body)
                    content = data.get("content", "")
                    debug_print(f"Extracted content: {len(content)} chars")
                    print(content)
                    success = True
                    break
                else:
                    errors.append(f"{base_url}: Status {response.status}")
                    debug_print(f"Failed with status: {response.status}")
        except urllib.error.HTTPError as e:
            body = e.read().decode('utf-8')
            error_msg = f"{base_url}: HTTP {e.code} {e.reason} - {body}"
            errors.append(error_msg)
            debug_print(f"HTTP Error: {error_msg}")
        except Exception as e:
            error_msg = f"{base_url}: {e}"
            errors.append(error_msg)
            debug_print(f"Exception: {error_msg}")
            
    if not success:
        print(f"Error: Failed to connect to Orchestrator.\nTried: {urls}\nErrors:\n" + "\n".join(errors), file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
