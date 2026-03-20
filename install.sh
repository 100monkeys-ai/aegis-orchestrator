#!/usr/bin/env bash
# Copyright (c) 2026 100monkeys.ai
# SPDX-License-Identifier: AGPL-3.0
#
# install.sh — One-shot AEGIS installer for Ubuntu / macOS
#
# Installs the Rust toolchain (if absent), builds and installs the
# aegis-orchestrator CLI via cargo, then runs `aegis up` to bring the full
# local stack online.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/100monkeys-ai/aegis-orchestrator/main/install.sh | bash
#   # or
#   chmod +x install.sh && ./install.sh

set -euo pipefail

AEGIS_VERSION="${AEGIS_VERSION:-0.12.0-pre-alpha}"

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
    for pkg in curl build-essential pkg-config libssl-dev ca-certificates; do
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
    # Ensure Homebrew is present
    if ! command -v brew &>/dev/null; then
        info "Homebrew not found — installing..."
        /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
        # Add brew to PATH for Apple Silicon or Intel
        if [[ -f /opt/homebrew/bin/brew ]]; then
            eval "$(/opt/homebrew/bin/brew shellenv)"
        else
            eval "$(/usr/local/bin/brew shellenv)"
        fi
    fi

    MISSING_BREW_PKGS=()
    for pkg in openssl pkg-config; do
        if ! brew list --versions "$pkg" >/dev/null 2>&1; then
            MISSING_BREW_PKGS+=("$pkg")
        fi
    done

    if [[ ${#MISSING_BREW_PKGS[@]} -eq 0 ]]; then
        success "System dependencies already installed. Skipping brew install."
    else
        info "Installing missing brew packages: ${MISSING_BREW_PKGS[*]}"
        brew install "${MISSING_BREW_PKGS[@]}"
    fi
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
    curl -fsSL https://download.docker.com/linux/ubuntu/gpg \
        | sudo gpg --dearmor -o /etc/apt/keyrings/docker.gpg
    sudo chmod a+r /etc/apt/keyrings/docker.gpg
    echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] \
https://download.docker.com/linux/ubuntu $(lsb_release -cs) stable" \
        | sudo tee /etc/apt/sources.list.d/docker.list > /dev/null
    sudo apt-get update -qq
    sudo apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
    # Add current user to the docker group so we don't need sudo
    sudo usermod -aG docker "$USER"
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

# ── Step 3: Rust / Cargo ────────────────────────────────────────────────────────
if command -v cargo &>/dev/null; then
    success "Rust toolchain already installed ($(cargo --version)). Skipping."
else
    info "Installing Rust via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --profile minimal
    success "Rust installed."
fi

# Make cargo available in the current shell whether it was just installed or
# already present in a non-login environment (e.g. piped bash).
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"

cargo --version &>/dev/null || die "cargo not found on PATH after install. Try: source \$HOME/.cargo/env"

# ── Step 4: Install aegis-orchestrator ────────────────────────────────────────
CURRENT_AEGIS_VERSION=""
if command -v aegis &>/dev/null; then
    CURRENT_AEGIS_VERSION="$(aegis --version 2>/dev/null | grep -Eo '[0-9]+\.[0-9]+\.[^ ]+' || true)"
fi

if [[ "$CURRENT_AEGIS_VERSION" == "$AEGIS_VERSION" ]]; then
    success "aegis $AEGIS_VERSION already installed. Skipping cargo install."
else
    info "Installing aegis-orchestrator CLI via cargo (version: $AEGIS_VERSION)..."
    if ! cargo install aegis-orchestrator --version "$AEGIS_VERSION"; then
        info "Failed to install aegis-orchestrator version $AEGIS_VERSION. Attempting to install latest version instead..."
        if ! cargo install aegis-orchestrator; then
            die "Unable to install aegis-orchestrator. Both version $AEGIS_VERSION and latest version failed. Please check your network connection and cargo configuration."
        fi
    fi
    success "aegis installed: $(aegis --version)"
fi

# Ensure the cargo bin dir is on PATH for the aegis up call and future sessions.
CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"
if [[ ":$PATH:" != *":$CARGO_BIN:"* ]]; then
    export PATH="$CARGO_BIN:$PATH"
fi

# Persist to shell profile so it survives reboots.
# Write to every RC file that exists (bash + zsh) so it works regardless of
# which shell the user opens next.
_add_cargo_to_rc() {
    local rc="$1"
    if [[ -f "$rc" ]] && ! grep -q '# Added by AEGIS installer' "$rc" 2>/dev/null; then
        echo '' >> "$rc"
        echo '# Added by AEGIS installer' >> "$rc"
        echo "export PATH=\"\$HOME/.cargo/bin:\$PATH\"" >> "$rc"
        info "Added ~/.cargo/bin to PATH in $rc"
    fi
}
_add_cargo_to_rc "$HOME/.bashrc"
_add_cargo_to_rc "$HOME/.zshrc"
if [[ "$PLATFORM" == "macos" ]]; then
    _add_cargo_to_rc "$HOME/.bash_profile"
fi

# ── Step 5: Start the AEGIS stack ─────────────────────────────────────────────
# Socket access is resolved dynamically inside the aegis-runtime container at startup.
info "Starting AEGIS stack (aegis up)..."
aegis up

success "AEGIS is ready! Run 'aegis --help' to get started."
