#!/usr/bin/env python3
# MODIFIED VERSION FOR TESTING ITERATIONS
# This version intentionally exits with code 1 to trigger iteration retries

import os
import sys
import json
import urllib.request
import urllib.error

def main():
    # 1. Configuration
    orchestrator_url = os.environ.get("AEGIS_ORCHESTRATOR_URL", "http://host.docker.internal:8000")
    agent_instruction = os.environ.get("AEGIS_AGENT_INSTRUCTION", "You are a helpful assistant.")
    agent_id = os.environ.get("AEGIS_AGENT_ID", "unknown")
    execution_id = os.environ.get("AEGIS_EXECUTION_ID")
    
    # 2. Read Input
    # Input is passed as the first argument or via stdin
    if len(sys.argv) > 1:
        user_input = sys.argv[1]
    else:
        # Fallback to stdin if needed, but for now we expect args
        user_input = sys.stdin.read().strip()

    if not user_input:
        print("Error: No input provided", file=sys.stderr)
        sys.exit(1)

    # 3. Construct Prompt
    # Simple concatenation for now. In future, we can support chat history.
    prompt = f"{agent_instruction}\n\nUser: {user_input}\nAssistant:"

    # 4. Call LLM Proxy with Failover
    payload = {
        "prompt": prompt,
        "execution_id": execution_id,
        "model": "default" # Or from env
    }

    # 4. Call LLM Proxy
    # We rely on DockerRuntime setting "host.docker.internal:host-gateway"
    # or the user providing AEGIS_ORCHESTRATOR_URL.
    
    payload = {
        "prompt": prompt,
        "execution_id": execution_id,
        "model": "default"
    }
    
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
        url = f"{base_url.rstrip('/')}/api/llm/generate"
        try:
            req = urllib.request.Request(
                url,
                data=payload_data,
                headers=headers
            )
            with urllib.request.urlopen(req, timeout=10) as response:
                if 200 <= response.status < 300:
                    body = response.read().decode('utf-8')
                    data = json.loads(body)
                    content = data.get("content", "")
                    print(content)
                    success = True
                    break
                else:
                    errors.append(f"{base_url}: Status {response.status}")
        except urllib.error.HTTPError as e:
            body = e.read().decode('utf-8')
            errors.append(f"{base_url}: HTTP {e.code} {e.reason} - {body}")
        except Exception as e:
            errors.append(f"{base_url}: {e}")
            
    if not success:
        print(f"Error: Failed to connect to Orchestrator.\nTried: {urls}\nErrors:\n" + "\n".join(errors), file=sys.stderr)
        sys.exit(1)

    # TESTING: Intentionally exit with code 1 to trigger iteration retries
    print("TEST: Intentionally failing to trigger iteration", file=sys.stderr)
    sys.exit(1)

if __name__ == "__main__":
    main()
