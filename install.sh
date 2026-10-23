#!/usr/bin/env bash
# Copyright (c) 2026 100monkeys.ai
# SPDX-License-Identifier: AGPL-3.0
#
# install.sh — One-shot AEGIS installer for Ubuntu / macOS
#
# Installs the aegis CLI from a GitHub release tarball, then runs `aegis up`
# with the pinned version to bring the full local stack online.
#
# Usage:
#   curl -fsSL https://get.100monkeys.ai | bash
#   # or
#   chmod +x install.sh && ./install.sh

set -euo pipefail

PINNED_VERSION="1.1.0-beta"
AEGIS_VERSION="${AEGIS_VERSION:-$PINNED_VERSION}"

# ── Colours ───────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
BOLD='\033[1m'
RESET='\033[0m'

info()    { echo -e "${CYAN}${BOLD}[aegis]${RESET} $*"; }
success() { echo -e "${GREEN}${BOLD}[aegis]${RESET} $*"; }
die()     { echo -e "${RED}${BOLD}[aegis] ERROR:${RESET} $*" >&2; exit 1; }

if [[ "$AEGIS_VERSION" == "latest" ]]; then
    # Resolve "latest" to the most recent GitHub release tag.
    # Fall back to the pinned version if resolution fails.
    LATEST_TAG=""
    if command -v curl >/dev/null 2>&1; then
        GITHUB_API_URL="https://api.github.com/repos/100monkeys-ai/aegis-orchestrator/releases?per_page=1"
        RAW_RELEASE_JSON="$(curl -fsSL "$GITHUB_API_URL" 2>/dev/null || true)"
        if [[ -n "${RAW_RELEASE_JSON:-}" ]]; then
            if command -v jq >/dev/null 2>&1; then
                LATEST_TAG="$(printf '%s' "$RAW_RELEASE_JSON" | jq -r '.[0].tag_name // empty' || true)"
            else
                # Minimal JSON parsing fallback if jq is unavailable.
                LATEST_TAG="$(printf '%s' "$RAW_RELEASE_JSON" | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/' || true)"
            fi
        fi
        if [[ -n "${LATEST_TAG}" ]]; then
            DOWNLOAD_VERSION="$LATEST_TAG"
            info "Resolved AEGIS_VERSION=latest to GitHub release tag: $DOWNLOAD_VERSION"
        else
            info "Could not resolve latest GitHub release; falling back to pinned version $PINNED_VERSION"
            DOWNLOAD_VERSION="$PINNED_VERSION"
        fi
    else
        info "curl not available to resolve latest release; falling back to pinned version $PINNED_VERSION"
        DOWNLOAD_VERSION="$PINNED_VERSION"
    fi
else
    DOWNLOAD_VERSION="$AEGIS_VERSION"
fi

# ── Detect OS ─────────────────────────────────────────────────────────────────
OS="$(uname -s)"
case "$OS" in
    Linux)  PLATFORM="linux" ;;
    Darwin) PLATFORM="macos" ;;
    *)      die "Unsupported OS: $OS. This script supports Ubuntu/Debian and macOS." ;;
esac
info "Detected platform: $PLATFORM"

# ── Step 1: System dependencies ───────────────────────────────────────────────
info "Installing system dependencies..."

if [[ "$PLATFORM" == "linux" ]]; then
    command -v apt-get &>/dev/null \
        || die "apt-get not found. Linux installs require Ubuntu / Debian."
    MISSING_APT_PKGS=()
    # fuse3: the released aegis binary links libfuse3.so.3 (its FUSE daemon).
    for pkg in curl ca-certificates fuse3; do
        if ! dpkg-query -W -f='${Status}' "$pkg" 2>/dev/null | grep -q "install ok installed"; then
            MISSING_APT_PKGS+=("$pkg")
        fi
    done

    if [[ ${#MISSING_APT_PKGS[@]} -eq 0 ]]; then
        success "System dependencies already installed. Skipping apt install."
    else
        info "Installing missing apt packages: ${MISSING_APT_PKGS[*]}"
        sudo apt-get update -qq
        sudo apt-get install -y --no-install-recommends "${MISSING_APT_PKGS[@]}"
    fi

elif [[ "$PLATFORM" == "macos" ]]; then
    success "macOS requires no extra system packages for the release binary install."
fi

# ── Step 1b: libfuse3.so.3 ─────────────────────────────────────────────
# The released aegis binary links libfuse3.so.3. Ubuntu 25.10 and later ship
# libfuse3-4 (libfuse3.so.4) and no libfuse3-3, so the library is put in place
# from Ubuntu's own pinned package when the distro cannot. Idempotent: skipped
# when the library is already present.
if [[ "$PLATFORM" == "linux" ]] && [[ "$(uname -m)" == "x86_64" ]]; then
    if ldconfig -p 2>/dev/null | grep -q 'libfuse3\.so\.3'; then
        success "libfuse3.so.3 present."
    elif apt-cache show libfuse3-3 &>/dev/null; then
        info "Installing libfuse3-3..."
        sudo apt-get install -y --no-install-recommends libfuse3-3
    else
        LIBFUSE_DEB_URL="http://archive.ubuntu.com/ubuntu/pool/main/f/fuse3/libfuse3-3_3.14.0-5build1_amd64.deb"
        LIBFUSE_DEB_SHA256="3042159c6d7d4dcb3df9d49cd21e49ecb57f4ba8c4153a72102f512a4fc534a2"
        info "This release has no libfuse3-3 package; installing Ubuntu's pinned libfuse3-3 3.14.0-5build1..."
        LIBFUSE_DEB="$(mktemp /tmp/libfuse3-3.XXXXXX.deb)"
        curl -fsSL -o "$LIBFUSE_DEB" "$LIBFUSE_DEB_URL"
        echo "$LIBFUSE_DEB_SHA256  $LIBFUSE_DEB" | sha256sum -c - >/dev/null \
            || die "libfuse3-3 download did not match its pinned sha256."
        sudo dpkg -i "$LIBFUSE_DEB" >/dev/null
        rm -f "$LIBFUSE_DEB"
        ldconfig -p 2>/dev/null | grep -q 'libfuse3\.so\.3' || die "libfuse3.so.3 still missing after install."
        success "libfuse3.so.3 installed."
    fi
fi

# Start the Docker engine and make it reachable from this shell, without
# Docker Desktop: systemd or the service script, then the docker group for
# the current user, activated for this session with sg. Idempotent.
DOCKER_GROUP_PREFIX=""
ensure_docker_engine() {
    # A leftover Docker Desktop context would point the client at a socket
    # that no longer exists; the engine installed here answers on the default
    # context. DOCKER_HOST, when the person set it, is respected.
    if [[ -z "${DOCKER_HOST:-}" ]]; then
        export DOCKER_CONTEXT=default
    fi
    if docker info &>/dev/null; then
        return 0
    fi
    # On macOS the daemon is Docker Desktop's; the Linux steps below do not apply.
    if [[ "$PLATFORM" == "macos" ]]; then
        die "Docker Desktop is installed but not running. Start Docker Desktop, wait until it is ready, and rerun this installer."
    fi
    info "Docker daemon is not reachable. Starting the engine..."
    if [[ -d /run/systemd/system ]] && command -v systemctl &>/dev/null; then
        sudo systemctl enable --now docker 2>/dev/null || sudo systemctl start docker 2>/dev/null || true
    else
        sudo service docker start 2>/dev/null || true
    fi
    local i
    for i in $(seq 1 15); do
        sudo docker info &>/dev/null && break
        sleep 1
    done
    # WSL without systemd: dockerd often dies at once on nftables-backed
    # iptables ("iptables ... nf_tables" in its log). Switch to the legacy
    # backend once and start again. Idempotent: the alternative stays set.
    if ! sudo docker info &>/dev/null && [[ "$IS_WSL" == "true" ]] \
        && sudo grep -qiE 'iptables|nf_tables|nftables' /var/log/docker.log 2>/dev/null \
        && [[ -x /usr/sbin/iptables-legacy ]]; then
        info "dockerd failed on nftables iptables; switching to iptables-legacy and starting again..."
        sudo update-alternatives --set iptables /usr/sbin/iptables-legacy >/dev/null 2>&1 || true
        sudo update-alternatives --set ip6tables /usr/sbin/ip6tables-legacy >/dev/null 2>&1 || true
        sudo service docker start 2>/dev/null || true
        # shellcheck disable=SC2034 # i only counts the fifteen one-second retries
        for i in $(seq 1 15); do
            sudo docker info &>/dev/null && break
            sleep 1
        done
    fi
    if ! sudo docker info &>/dev/null; then
        echo ""
        echo -e "${RED}${BOLD}[aegis] ERROR:${RESET} the Docker engine did not start. Its log says:"
        sudo tail -n 20 /var/log/docker.log 2>/dev/null | sed 's/^/        /' \
            || sudo journalctl -u docker --no-pager -n 20 2>/dev/null | sed 's/^/        /' \
            || echo "        (no /var/log/docker.log and no journal)"
        if [[ "$IS_WSL" == "true" ]]; then
            echo -e "        In WSL: ${BOLD}sudo service docker start${RESET} (or enable systemd in /etc/wsl.conf), then run this installer again."
        else
            echo -e "        Start it (${BOLD}sudo systemctl start docker${RESET}), then run this installer again."
        fi
        echo ""
        exit 1
    fi
    if docker info &>/dev/null; then
        success "Docker daemon started."
        return 0
    fi
    # The engine runs; this user cannot reach the socket in this session yet.
    if ! id -nG "$USER" | tr ' ' '\n' | grep -qx docker; then
        sudo usermod -aG docker "$USER"
    fi
    if sg docker -c "docker info" &>/dev/null; then
        DOCKER_GROUP_PREFIX="sg docker -c"
        success "Docker daemon running; the docker group applies to this session through sg, and to new shells on their own."
    else
        echo ""
        echo -e "${RED}${BOLD}[aegis] ERROR:${RESET} the Docker engine runs (root reaches it) but this user does not. The client says:"
        sg docker -c "docker info" 2>&1 | grep -v -E '^(Client|Server| )' | head -n 5 | sed 's/^/        /'
        echo -e "        socket: $(ls -l /var/run/docker.sock 2>&1)"
        echo -e "        groups: $(id -nG "$USER")"
        echo -e "        Fix the cause above (or run 'docker info' in a new login shell), then run this installer again."
        echo ""
        exit 1
    fi
}

# ── Step 2: Docker ────────────────────────────────────────────────────────────
info "Checking Docker..."

# Detect WSL2
IS_WSL=false
if grep -qiE 'microsoft|WSL' /proc/version 2>/dev/null; then
    IS_WSL=true
fi

if command -v docker &>/dev/null; then
    ensure_docker_engine
    success "Docker is running ($(docker --version 2>/dev/null || sudo docker --version))."
elif [[ "$PLATFORM" == "linux" ]]; then
    if [[ "$IS_WSL" == "true" ]]; then
        echo ""
        echo -e "${CYAN}${BOLD}[aegis] WSL detected.${RESET} Docker Engine is not installed in this WSL distro."
        echo -e "        Installing Docker Engine (docker-ce) inside WSL; no Docker Desktop needed."
        echo -e "          https://docs.docker.com/desktop/wsl/"
        echo ""
        echo -e "        Alternatively, install Docker Engine natively inside WSL:"
        echo ""
    fi
    info "Installing Docker Engine..."
    sudo apt-get update -qq
    sudo apt-get install -y --no-install-recommends gnupg lsb-release
    sudo install -m 0755 -d /etc/apt/keyrings
    # Determine correct Docker repo base for Ubuntu vs Debian
    DIST_ID="$(lsb_release -is 2>/dev/null | tr '[:upper:]' '[:lower:]')"
    case "$DIST_ID" in
        ubuntu)
            DOCKER_REPO_BASE="https://download.docker.com/linux/ubuntu"
            ;;
        debian)
            DOCKER_REPO_BASE="https://download.docker.com/linux/debian"
            ;;
        *)
            die "Unsupported distribution '$DIST_ID' for automatic Docker installation. Supported: Ubuntu, Debian."
            ;;
    esac
    curl -fsSL "${DOCKER_REPO_BASE}/gpg" \
        | sudo gpg --dearmor -o /etc/apt/keyrings/docker.gpg
    sudo chmod a+r /etc/apt/keyrings/docker.gpg
    echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] \
${DOCKER_REPO_BASE} $(lsb_release -cs) stable" \
        | sudo tee /etc/apt/sources.list.d/docker.list > /dev/null
    sudo apt-get update -qq
    sudo apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
    ensure_docker_engine
    success "Docker installed and running."
elif [[ "$PLATFORM" == "macos" ]]; then
    die "Docker is not running. Please install and start Docker Desktop: https://docs.docker.com/desktop/mac/install/"
fi

normalize_arch() {
    local arch
    arch="$(uname -m)"
    case "$arch" in
        x86_64|amd64) echo "x86_64" ;;
        arm64|aarch64) echo "aarch64" ;;
        *)
            die "Unsupported CPU architecture: $arch."
            ;;
    esac
}

install_aegis_from_release() {
    local arch asset url tmpdir extract_dir bin_path install_dir install_path
    tmpdir="$(mktemp -d)"
    cleanup_tmpdir() {
        rm -rf "$tmpdir"
    }
    trap cleanup_tmpdir RETURN

    arch="$(normalize_arch)"
    case "${PLATFORM}:${arch}" in
        linux:x86_64)
            asset="aegis-linux-x86_64.tar.gz"
            ;;
        macos:aarch64)
            asset="aegis-macos-aarch64.tar.gz"
            ;;
        *)
            die "No release asset available for ${PLATFORM}/${arch}. Supported combinations are linux/x86_64 and macos/aarch64 (Apple Silicon). Note: macOS on Intel (x86_64) is not supported."
            ;;
    esac

    if command -v aegis &>/dev/null; then
        local current_version
        current_version="$(aegis --version 2>/dev/null | grep -Eo '[0-9]+\.[0-9]+\.[^ ]+' || true)"
        if [[ "$current_version" == "$DOWNLOAD_VERSION" ]]; then
            success "aegis $DOWNLOAD_VERSION already installed. Skipping release download."
            if [[ -z "${AEGIS_BIN_PATH:-}" ]]; then
                AEGIS_BIN_PATH="$(command -v aegis)"
            fi
            return
        fi
    fi

    url="https://github.com/100monkeys-ai/aegis-orchestrator/releases/download/${DOWNLOAD_VERSION}/${asset}"
    extract_dir="$tmpdir/extract"
    mkdir -p "$extract_dir"

    info "Downloading aegis ${DOWNLOAD_VERSION} (${asset})..."
    curl -fsSL "$url" -o "$tmpdir/$asset"

    info "Extracting release archive..."
    tar -xzf "$tmpdir/$asset" -C "$extract_dir"

    bin_path=""
    if [[ -x "$extract_dir/aegis" ]]; then
        bin_path="$extract_dir/aegis"
    elif [[ -x "$extract_dir/bin/aegis" ]]; then
        bin_path="$extract_dir/bin/aegis"
    else
        bin_path="$(find "$extract_dir" -type f -name aegis -perm -111 | head -n 1 || true)"
    fi

    [[ -n "$bin_path" ]] || die "Downloaded archive did not contain an executable aegis binary."

    install_dir="/usr/local/bin"
    sudo install -d -m 0755 "$install_dir"
    install_path="$install_dir/aegis"
    sudo install -m 0755 "$bin_path" "$install_path"

    success "Installed aegis to $install_path"
    AEGIS_BIN_PATH="$install_path"
}

# ── Step 3: Install aegis release binary ──────────────────────────────────────
install_aegis_from_release

# ── Step 4: Start the AEGIS stack ─────────────────────────────────────────────
# Socket access is resolved dynamically inside the aegis-runtime container at startup.
info "Starting AEGIS stack (aegis up --tag $DOWNLOAD_VERSION)..."
if [[ -n "$DOCKER_GROUP_PREFIX" ]]; then
    sg docker -c "DOCKER_CONTEXT=${DOCKER_CONTEXT:-default} \"$AEGIS_BIN_PATH\" up --tag \"$DOWNLOAD_VERSION\""
else
    "$AEGIS_BIN_PATH" up --tag "$DOWNLOAD_VERSION"
fi

success "AEGIS is ready!"
info "Run 'aegis --help' to get started."
info "To update in the future:"
info "  1) Rerun this installer script to install the latest aegis CLI version."
info "  2) Run 'aegis update' to apply the update to your stack."
