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
#   curl -fsSL https://raw.githubusercontent.com/100monkeys-ai/aegis-orchestrator/main/install.sh | bash
#   # or
#   chmod +x install.sh && ./install.sh

set -euo pipefail

PINNED_VERSION="0.12.0-pre-alpha"
AEGIS_VERSION="${AEGIS_VERSION:-$PINNED_VERSION}"

if [[ "$AEGIS_VERSION" == "latest" ]]; then
    DOWNLOAD_VERSION="$PINNED_VERSION"
else
    DOWNLOAD_VERSION="$AEGIS_VERSION"
fi

# ── Colours ───────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
BOLD='\033[1m'
RESET='\033[0m'

info()    { echo -e "${CYAN}${BOLD}[aegis]${RESET} $*"; }
success() { echo -e "${GREEN}${BOLD}[aegis]${RESET} $*"; }
die()     { echo -e "${RED}${BOLD}[aegis] ERROR:${RESET} $*" >&2; exit 1; }

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
    for pkg in curl ca-certificates; do
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

# ── Step 2: Docker ────────────────────────────────────────────────────────────
info "Checking Docker..."

# Detect WSL2
IS_WSL=false
if grep -qiE 'microsoft|WSL' /proc/version 2>/dev/null; then
    IS_WSL=true
fi

if command -v docker &>/dev/null; then
    if docker info &>/dev/null 2>&1; then
        success "Docker is running ($(docker --version))."
    else
        info "Docker is installed but daemon is not reachable. Attempting to start daemon..."
        if command -v systemctl &>/dev/null; then
            sudo systemctl start docker 2>/dev/null || true
        else
            sudo service docker start 2>/dev/null || true
        fi

        if docker info &>/dev/null 2>&1; then
            success "Docker daemon started successfully."
        else
            echo ""
            echo -e "${RED}${BOLD}[aegis] ERROR:${RESET} Docker is installed but not reachable for current user/session."
            if [[ "$IS_WSL" == "true" ]]; then
                echo -e "        WSL detected. Ensure Docker Desktop WSL integration is enabled:"
                echo -e "          https://docs.docker.com/desktop/wsl/"
            fi
            echo -e "        Try:"
            echo -e "          - restarting Docker Desktop / Docker daemon"
            echo -e "          - re-login after docker group changes"
            echo -e "          - verifying: ${BOLD}docker info${RESET}"
            echo ""
            exit 1
        fi
    fi
elif [[ "$PLATFORM" == "linux" ]]; then
    if [[ "$IS_WSL" == "true" ]]; then
        echo ""
        echo -e "${CYAN}${BOLD}[aegis] WSL detected.${RESET} Docker Engine is not installed in this WSL distro."
        echo -e "        Docker Desktop with WSL integration is the easiest option:"
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
    # Add current user to the docker group so we don't need sudo
    sudo usermod -aG docker "$USER"
    echo ""
    echo -e "${CYAN}${BOLD}[aegis] NOTE:${RESET} You may need to log out and back in (or restart your terminal)"
    echo -e "        before using ${BOLD}docker${RESET} without ${BOLD}sudo${RESET}."
    # Start the daemon (best-effort; may fail in some WSL environments)
    sudo service docker start 2>/dev/null || true
    # Verify
    if ! docker info &>/dev/null 2>&1; then
        echo ""
        echo -e "${RED}${BOLD}[aegis] WARNING:${RESET} Docker daemon is not reachable after install."
        if [[ "$IS_WSL" == "true" ]]; then
            echo -e "        In WSL you may need to start it manually: ${BOLD}sudo service docker start${RESET}"
            echo -e "        Or enable Docker Desktop WSL integration: https://docs.docker.com/desktop/wsl/"
        fi
        echo ""
    else
        success "Docker installed and running."
    fi
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
            die "No release asset available for ${PLATFORM}/${arch}. Supported combinations are linux/x86_64 and macos/aarch64."
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
info "Starting AEGIS stack (aegis up --tag $AEGIS_VERSION)..."
"$AEGIS_BIN_PATH" up --tag "$AEGIS_VERSION"

success "AEGIS is ready!"
info "Run 'aegis --help' to get started."
info "To update in the future:"
info "  1) Rerun this installer script to install the latest aegis CLI version."
info "  2) Run 'aegis update' to apply the update to your stack."
