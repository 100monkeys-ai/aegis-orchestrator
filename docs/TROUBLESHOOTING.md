# Troubleshooting Guide

Common issues and solutions when working with AEGIS.

## Table of Contents

- [Daemon Issues](#daemon-issues)
- [Configuration Issues](#configuration-issues)
- [Agent Deployment Issues](#agent-deployment-issues)
- [Execution Issues](#execution-issues)
- [Docker Issues](#docker-issues)
- [LLM Provider Issues](#llm-provider-issues)
- [Log Files](#log-files)
- [Getting Help](#getting-help)

## Daemon Issues

### Daemon Won't Start

**Symptom:** `aegis daemon start` fails or daemon immediately stops.

**Solutions:**

1. **Check if daemon is already running:**

   ```bash
   aegis daemon status
   ```

   If running, stop it first:

   ```bash
   aegis daemon stop
   ```

2. **Check port availability:**

   ```bash
   # Linux/macOS
   lsof -i :8000
   
   # Windows
   netstat -ano | findstr :8000
   ```

   If port 8000 is in use, start on different port:

   ```bash
   aegis --port 9000 daemon start
   ```

3. **Check daemon logs:**

   ```bash
   # Linux/macOS
   cat /tmp/aegis.out
   cat /tmp/aegis.err
   
   # Windows
   type %TEMP%\aegis.out
   type %TEMP%\aegis.err
   ```

4. **Verify configuration:**

   ```bash
   aegis config validate
   ```

---

### Daemon Status Shows "Unhealthy"

**Symptom:** `aegis daemon status` shows daemon is unhealthy.

**Solutions:**

1. **Check daemon logs:**

   ```bash
   tail -f /tmp/aegis.err
   ```

2. **Restart daemon:**

   ```bash
   aegis daemon stop --force
   aegis daemon start
   ```

3. **Check Docker is running:**

   ```bash
   docker ps
   ```

4. **Verify network connectivity:**

   ```bash
   curl http://localhost:8000/health
   ```

---

### Can't Stop Daemon

**Symptom:** `aegis daemon stop` doesn't stop the daemon.

**Solutions:**

1. **Force stop:**

   ```bash
   aegis daemon stop --force --timeout 10
   ```

2. **Kill process manually:**

   ```bash
   # Linux/macOS
   pkill aegis
   
   # Windows
   taskkill /F /IM aegis.exe
   ```

3. **Find and kill by PID:**

   ```bash
   # Linux/macOS
   ps aux | grep aegis
   kill -9 <PID>
   
   # Windows
   Get-Process aegis | Stop-Process -Force
   ```

## Configuration Issues

### "No LLM Providers Configured" Warning

**Symptom:** Warning when starting daemon about missing LLM providers.

**Solutions:**

1. **Add LLM provider to config:**

   ```yaml
   llm_providers:
     - name: "local"
       type: "ollama"
       endpoint: "http://localhost:11434"
       models:
         - alias: "default"
           model: "phi3:mini"
   ```

2. **Verify Ollama is running:**

   ```bash
   curl http://localhost:11434/api/tags
   ```

3. **Pull required model:**

   ```bash
   ollama pull phi3:mini
   ```

---

### Configuration File Not Found

**Symptom:** `Failed to load configuration` error.

**Solutions:**

1. **Create config file:**

   ```bash
   cp aegis-config.yaml my-config.yaml
   ```

2. **Specify config path:**

   ```bash
   aegis --config ./my-config.yaml daemon start
   ```

3. **Use environment variable:**

   ```bash
   export AEGIS_CONFIG_PATH=./my-config.yaml
   aegis daemon start
   ```

---

### Invalid Configuration

**Symptom:** Configuration validation errors.

**Solutions:**

1. **Validate config:**

   ```bash
   aegis config validate --config ./my-config.yaml
   ```

2. **Check YAML syntax:**
   - Ensure proper indentation (2 spaces)
   - No tabs, only spaces
   - Proper quoting of strings

3. **Use example config:**

   ```bash
   aegis config generate > new-config.yaml
   ```

## Agent Deployment Issues

### Agent Deployment Fails

**Symptom:** `aegis agent deploy` returns an error.

**Solutions:**

1. **Verify manifest syntax:**

   ```bash
   # Check YAML is valid
   python -c "import yaml; yaml.safe_load(open('agent.yaml'))"
   ```

2. **Check required fields:**

   ```yaml
   version: "1.1"
   agent:
     name: "my-agent"    # Required
     runtime: "python:3.11"  # Required
   task:
     instruction: "..."  # Required
   ```

3. **Ensure daemon is running:**

   ```bash
   aegis daemon status
   ```

4. **Check agent name uniqueness:**

   ```bash
   aegis agent list
   ```

   If agent exists, remove it first:

   ```bash
   aegis agent remove <agent-id>
   ```

---

### "Agent Already Exists" Error

**Symptom:** Cannot deploy agent with same name.

**Solutions:**

1. **List existing agents:**

   ```bash
   aegis agent list
   ```

2. **Remove existing agent:**

   ```bash
   aegis agent remove <agent-id>
   ```

3. **Use different name:**

   ```yaml
   agent:
     name: "my-agent-v2"
   ```

## Execution Issues

### Task Execution Hangs

**Symptom:** `aegis task execute` never completes.

**Solutions:**

1. **Check execution status:**

   ```bash
   aegis task list
   aegis task status <execution-id>
   ```

2. **View execution logs:**

   ```bash
   aegis task logs <execution-id> --follow
   ```

3. **Cancel execution:**

   ```bash
   aegis task cancel <execution-id>
   ```

4. **Check timeout settings:**

   ```yaml
   execution:
     timeout_seconds: 120  # Increase if needed
   ```

---

### Task Execution Fails Immediately

**Symptom:** Execution fails right after starting.

**Solutions:**

1. **Check agent logs:**

   ```bash
   aegis agent logs <agent-name>
   ```

2. **Verify LLM provider is working:**

   ```bash
   # For Ollama
   curl http://localhost:11434/api/tags
   
   # Test model
   ollama run phi3:mini "Hello"
   ```

3. **Check Docker container:**

   ```bash
   docker ps -a
   docker logs <container-id>
   ```

4. **Verify input format:**

   ```bash
   # Use valid JSON
   aegis task execute my-agent --input '{"key": "value"}'
   ```

---

### "Agent Not Found" Error

**Symptom:** Cannot execute task for agent.

**Solutions:**

1. **List deployed agents:**

   ```bash
   aegis agent list
   ```

2. **Use correct agent name or ID:**

   ```bash
   # By name
   aegis task execute echo --input "test"
   
   # By ID
   aegis task execute a1b2c3d4-... --input "test"
   ```

3. **Deploy agent first:**

   ```bash
   aegis agent deploy ./agent.yaml
   ```

## Docker Issues

### Docker Not Running

**Symptom:** `Cannot connect to Docker daemon` error.

**Solutions:**

1. **Start Docker:**

   ```bash
   # Linux
   sudo systemctl start docker
   
   # macOS
   open -a Docker
   
   # Windows
   # Start Docker Desktop
   ```

2. **Verify Docker is running:**

   ```bash
   docker ps
   ```

3. **Check Docker permissions (Linux):**

   ```bash
   sudo usermod -aG docker $USER
   # Log out and back in
   ```

---

### Docker Image Pull Fails

**Symptom:** Cannot pull runtime image.

**Solutions:**

1. **Enable autopull in manifest:**

   ```yaml
   agent:
     autopull: true
   ```

2. **Pull image manually:**

   ```bash
   docker pull python:3.11
   ```

3. **Check network connectivity:**

   ```bash
   ping docker.io
   ```

4. **Use different image registry:**

   ```yaml
   runtime: "docker.io/library/python:3.11"
   ```

---

### Container Exits Immediately

**Symptom:** Docker container starts and stops right away.

**Solutions:**

1. **Check container logs:**

   ```bash
   docker ps -a
   docker logs <container-id>
   ```

2. **Verify runtime image:**

   ```bash
   docker run -it python:3.11 python --version
   ```

3. **Check agent manifest:**

   ```yaml
   agent:
     runtime: "python:3.11"  # Ensure valid image
   ```

## LLM Provider Issues

### Ollama Connection Failed

**Symptom:** Cannot connect to Ollama.

**Solutions:**

1. **Verify Ollama is running:**

   ```bash
   curl http://localhost:11434/api/tags
   ```

2. **Start Ollama:**

   ```bash
   ollama serve
   ```

3. **Check endpoint in config:**

   ```yaml
   llm_providers:
     - name: "local"
       endpoint: "http://localhost:11434"  # Correct endpoint
   ```

4. **Test Ollama directly:**

   ```bash
   ollama run phi3:mini "Hello"
   ```

---

### Model Not Found

**Symptom:** `Model not found` error.

**Solutions:**

1. **List available models:**

   ```bash
   ollama list
   ```

2. **Pull required model:**

   ```bash
   ollama pull phi3:mini
   ```

3. **Update config with available model:**

   ```yaml
   models:
     - alias: "default"
       model: "phi3:mini"  # Use available model
   ```

---

### OpenAI API Key Issues

**Symptom:** OpenAI authentication errors.

**Solutions:**

1. **Set API key in config:**

   ```yaml
   llm_providers:
     - name: "openai"
       type: "openai"
       api_key: "sk-..."
   ```

2. **Use environment variable:**

   ```bash
   export OPENAI_API_KEY="sk-..."
   ```

3. **Verify API key:**

   ```bash
   curl https://api.openai.com/v1/models \
     -H "Authorization: Bearer $OPENAI_API_KEY"
   ```

## Log Files

### Daemon Logs

**Linux/macOS:**

- stdout: `/tmp/aegis.out`
- stderr: `/tmp/aegis.err`

**Windows:**

- stdout: `%TEMP%\aegis.out`
- stderr: `%TEMP%\aegis.err`

### Viewing Logs

```bash
# Linux/macOS
tail -f /tmp/aegis.out
tail -f /tmp/aegis.err

# Windows PowerShell
Get-Content $env:TEMP\aegis.out -Wait
Get-Content $env:TEMP\aegis.err -Wait
```

### Agent Logs

```bash
# View agent logs
aegis agent logs <agent-name>

# Stream logs
aegis agent logs <agent-name> --follow

# View errors only
aegis agent logs <agent-name> --errors
```

### Execution Logs

```bash
# View execution logs
aegis task logs <execution-id>

# Stream logs
aegis task logs <execution-id> --follow
```

## Getting Help

### Enable Debug Logging

```bash
# Start daemon with debug logging
aegis --log-level debug daemon start

# Or set environment variable
export AEGIS_LOG_LEVEL=debug
aegis daemon start
```

### Collect Diagnostic Information

```bash
# System info
uname -a
docker --version
cargo --version

# Daemon status
aegis daemon status

# Configuration
aegis config show

# Agent list
aegis agent list

# Recent executions
aegis task list

# Logs
cat /tmp/aegis.out
cat /tmp/aegis.err
```

### Report Issues

When reporting issues, include:

1. **AEGIS version:**

   ```bash
   aegis --version
   ```

2. **System information:**
   - OS and version
   - Docker version
   - Rust version

3. **Configuration:**

   ```bash
   aegis config show
   ```

4. **Error messages:**
   - Full error output
   - Relevant log excerpts

5. **Steps to reproduce:**
   - Exact commands run
   - Agent manifest (if applicable)

---

**See Also:**

- [Getting Started Guide](GETTING_STARTED.md)
- [CLI Reference](CLI_REFERENCE.md)
- [Local Testing Guide](LOCAL_TESTING.md)
- [Agent Development Guide](AGENT_DEVELOPMENT.md)
