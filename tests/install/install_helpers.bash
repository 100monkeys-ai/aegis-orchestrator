# SPDX-License-Identifier: AGPL-3.0-only
# Copyright 2026 100monkeys.ai
#
# Helpers for the install.sh harness. Each test runs one block of install.sh,
# cut out of the file by its own text (never by line number), under
#   bash --norc --noprofile -euo pipefail
# with PATH set to a per-test shim directory. No network, no sudo, no docker.
#
# Written for bats 1.2 (the ubuntu-22.04 package): no bats_require_minimum_version,
# no run --separate-stderr, no BATS_TEST_TMPDIR.

# shellcheck disable=SC2154 # status and output are set by bats' run

# The installer under test. AEGIS_INSTALL_SH points the harness at another copy
# (an older revision, for a red-watch); the default is the repository's own.
INSTALL_SH="${AEGIS_INSTALL_SH:-$BATS_TEST_DIRNAME/../../install.sh}"

harness_setup() {
    [[ -f "$INSTALL_SH" ]] || { echo "install.sh not found at $INSTALL_SH" >&2; return 1; }
    WORK="$(mktemp -d "${BATS_TMPDIR:-/tmp}/aegis-install-test.XXXXXX")"
    SHIM="$WORK/bin"
    mkdir -p "$SHIM"
    CURL_LOG="$WORK/curl.calls"
    SUDO_LOG="$WORK/sudo.calls"
    ERR="$WORK/stderr"
    BASH_BIN="$(command -v bash)"
    # The fallback parser runs grep and sed; the block needs nothing else from
    # the system. curl, jq, docker and sudo are absent unless a test adds a fake.
    ln -s "$(command -v grep)" "$SHIM/grep"
    ln -s "$(command -v sed)" "$SHIM/sed"
}

harness_teardown() {
    [[ -n "${WORK:-}" && -d "$WORK" ]] && rm -rf "$WORK"
    return 0
}

# One line of install.sh that starts with the given text, or a refusal naming it.
installer_line() {
    local prefix="$1" line
    line="$(awk -v p="$prefix" 'index($0, p) == 1 { print; exit }' "$INSTALL_SH")"
    if [[ -z "$line" ]]; then
        echo "install.sh has no line starting with: $prefix" >&2
        return 1
    fi
    printf '%s\n' "$line"
}

# The latest-release resolution block: from its opening `if` to the `fi` that
# follows `DOWNLOAD_VERSION="$AEGIS_VERSION"`.
resolution_block() {
    awk '
        $0 == "if [[ \"$AEGIS_VERSION\" == \"latest\" ]]; then" { on = 1 }
        on { print }
        on && $0 == "    DOWNLOAD_VERSION=\"$AEGIS_VERSION\"" { seen = 1; next }
        on && seen && $0 == "fi" { done = 1; exit }
        END { if (!done) exit 1 }
    ' "$INSTALL_SH" || {
        echo "install.sh: the resolution block (if [[ \"\$AEGIS_VERSION\" == \"latest\" ]]; then ... fi) was not found whole" >&2
        return 1
    }
}

# ensure_docker_engine, from its `ensure_docker_engine() {` line to the first
# `}` at column 1 after it.
docker_function() {
    awk '
        $0 == "ensure_docker_engine() {" { on = 1 }
        on { print }
        on && $0 == "}" { done = 1; exit }
        END { if (!done) exit 1 }
    ' "$INSTALL_SH" || {
        echo "install.sh: ensure_docker_engine() { ... } was not found whole" >&2
        return 1
    }
}

# The pinned version as install.sh declares it.
pinned_version() {
    installer_line 'PINNED_VERSION="' | sed -E 's/^PINNED_VERSION="([^"]*)".*/\1/'
}

# A fake curl that records its arguments and prints the given body.
fake_curl() {
    local body="$1"
    printf '%s\n' "$body" > "$WORK/curl.body"
    cat > "$SHIM/curl" <<EOF
#!$BASH_BIN
printf '%s\n' "\$*" >> "$CURL_LOG"
while IFS= read -r line || [[ -n "\$line" ]]; do printf '%s' "\$line"; done < "$WORK/curl.body"
EOF
    chmod +x "$SHIM/curl"
}

# A fake jq that answers `.[0].tag_name // empty` for a one-release array and,
# like jq, fails with a parse error on anything that is not JSON of that shape.
fake_jq() {
    cat > "$SHIM/jq" <<EOF
#!$BASH_BIN
input=""
while IFS= read -r line || [[ -n "\$line" ]]; do input+="\$line"; done
if [[ "\$input" =~ ^\[\{\"tag_name\":\"([^\"]+)\"\}\]\$ ]]; then
    printf '%s\n' "\${BASH_REMATCH[1]}"
    exit 0
fi
echo "jq: error (at <stdin>:1): Invalid literal" >&2
exit 5
EOF
    chmod +x "$SHIM/jq"
}

# A fake docker whose every command fails, as `docker info` does when the
# daemon is down.
fake_docker_down() {
    printf '#!%s\nexit 1\n' "$BASH_BIN" > "$SHIM/docker"
    chmod +x "$SHIM/docker"
}

# A fake sudo that records any call and fails.
fake_sudo() {
    cat > "$SHIM/sudo" <<EOF
#!$BASH_BIN
printf '%s\n' "\$*" >> "$SUDO_LOG"
exit 1
EOF
    chmod +x "$SHIM/sudo"
}

# Writes a script running the resolution block, with install.sh's own
# PINNED_VERSION and info() lines in front, and prints DOWNLOAD_VERSION after.
resolution_script() {
    local script="$WORK/resolution.sh"
    {
        echo 'CYAN=""; BOLD=""; RESET=""'
        installer_line 'PINNED_VERSION="'
        installer_line 'info()'
        resolution_block
        # shellcheck disable=SC2016 # written into the script, expanded when it runs
        echo 'echo "DOWNLOAD_VERSION=$DOWNLOAD_VERSION"'
    } > "$script" || return 1
    printf '%s\n' "$script"
}

# Writes a script defining install.sh's helpers and ensure_docker_engine for
# the given platform, then calling it.
docker_script() {
    local platform="$1" script="$WORK/docker.sh"
    {
        echo 'RED=""; GREEN=""; CYAN=""; BOLD=""; RESET=""'
        installer_line 'info()'
        installer_line 'success()'
        installer_line 'die()'
        echo "PLATFORM=$platform"
        echo 'IS_WSL=false'
        echo 'DOCKER_GROUP_PREFIX=""'
        docker_function
        echo 'ensure_docker_engine'
        echo 'echo "ensure_docker_engine returned"'
    } > "$script" || return 1
    printf '%s\n' "$script"
}

# Runs a generated script in a clean environment: PATH is the shim directory,
# the named variables are passed through, stderr goes to $ERR.
#   run_isolated <script> [NAME=value ...]
run_isolated() {
    local script="$1"
    shift
    env -i PATH="$SHIM" HOME="$WORK" USER=tester "$@" \
        "$BASH_BIN" --norc --noprofile -euo pipefail "$script" 2> "$ERR"
}

assert_status() {
    if [[ "$status" -ne "$1" ]]; then
        echo "expected exit status $1, got $status" >&2
        echo "stdout: $output" >&2
        echo "stderr: $(< "$ERR")" >&2
        return 1
    fi
}

assert_output_has() {
    if [[ "$output" != *"$1"* ]]; then
        echo "expected stdout to contain: $1" >&2
        echo "stdout: $output" >&2
        echo "stderr: $(< "$ERR")" >&2
        return 1
    fi
}

assert_stderr_has() {
    if [[ "$(< "$ERR")" != *"$1"* ]]; then
        echo "expected stderr to contain: $1" >&2
        echo "stderr: $(< "$ERR")" >&2
        return 1
    fi
}

assert_download_version() {
    assert_output_has "DOWNLOAD_VERSION=$1"
}
