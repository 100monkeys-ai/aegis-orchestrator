#!/usr/bin/env bats
# SPDX-License-Identifier: AGPL-3.0-only
# Copyright 2026 100monkeys.ai
#
# install.sh: the latest-release resolution block and the macOS Docker Desktop
# check, each cut out of install.sh by its own text and run under
# bash -euo pipefail with fake curl, jq, docker and sudo on PATH.

load install_helpers

setup() {
    harness_setup
}

teardown() {
    harness_teardown
}

@test "latest: malformed release JSON with jq falls back to the pinned version" {
    fake_curl '<html>API rate limit exceeded</html>'
    fake_jq
    script="$(resolution_script)"
    run run_isolated "$script" AEGIS_VERSION=latest
    assert_status 0
    assert_output_has "Could not resolve latest GitHub release; falling back to pinned version"
    assert_download_version "$(pinned_version)"
}

@test "latest: malformed release JSON without jq falls back to the pinned version" {
    fake_curl '<html>API rate limit exceeded</html>'
    script="$(resolution_script)"
    run run_isolated "$script" AEGIS_VERSION=latest
    assert_status 0
    assert_output_has "Could not resolve latest GitHub release; falling back to pinned version"
    assert_download_version "$(pinned_version)"
}

@test "latest: one release with jq resolves to its tag" {
    fake_curl '[{"tag_name":"9.9.9-test"}]'
    fake_jq
    script="$(resolution_script)"
    run run_isolated "$script" AEGIS_VERSION=latest
    assert_status 0
    assert_download_version "9.9.9-test"
}

@test "latest: one release without jq resolves to its tag" {
    fake_curl '[{"tag_name":"9.9.9-test"}]'
    script="$(resolution_script)"
    run run_isolated "$script" AEGIS_VERSION=latest
    assert_status 0
    assert_download_version "9.9.9-test"
}

@test "latest: no curl on PATH falls back to the pinned version" {
    script="$(resolution_script)"
    run run_isolated "$script" AEGIS_VERSION=latest
    assert_status 0
    assert_output_has "curl not available to resolve latest release; falling back to pinned version"
    assert_download_version "$(pinned_version)"
}

@test "an explicit AEGIS_VERSION is used as given and curl is never called" {
    fake_curl '[{"tag_name":"9.9.9-test"}]'
    fake_jq
    script="$(resolution_script)"
    run run_isolated "$script" AEGIS_VERSION=1.2.3-explicit
    assert_status 0
    assert_download_version "1.2.3-explicit"
    if [[ -e "$CURL_LOG" ]]; then
        echo "curl was called: $(< "$CURL_LOG")" >&2
        return 1
    fi
}

@test "macOS: Docker Desktop stopped dies with its sentence and never calls sudo" {
    fake_docker_down
    fake_sudo
    script="$(docker_script macos)"
    run run_isolated "$script"
    assert_status 1
    assert_stderr_has "Docker Desktop is installed but not running. Start Docker Desktop, wait until it is ready, and rerun this installer."
    if [[ -e "$SUDO_LOG" ]]; then
        echo "sudo was called: $(< "$SUDO_LOG")" >&2
        return 1
    fi
    if [[ "$output" == *"ensure_docker_engine returned"* ]]; then
        echo "ensure_docker_engine returned instead of dying" >&2
        return 1
    fi
}
