// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Prerequisite detection and installation step of `aegis init`.
//!
//! Checks for Docker and Docker Compose. On supported platforms it can auto-install missing tools via the
//! platform-native package manager; pass `manual = true` to skip auto-install
//! and print instructions instead.
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** prereqs step inside the `aegis init` wizard

use anyhow::{bail, Result};
use colored::Colorize;

/// Whether a given prerequisite binary was found and is usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrereqStatus {
    Found,
    Missing,
}

/// A single prerequisite (binary name + friendly display name + optional auto-install support).
struct Prereq {
    /// Binary name used by `which`
    binary: &'static str,
    /// Human-readable label
    label: &'static str,
    /// If true, auto-install is supported on at least one platform
    auto_installable: bool,
    /// Install instructions (printed when manual=true or auto-install fails/is unsupported)
    instructions: &'static str,
}

const PREREQS: &[Prereq] = &[
    Prereq {
        binary: "docker",
        label: "Docker",
        auto_installable: true,
        instructions: "Install Docker Desktop from https://docs.docker.com/get-docker/",
    },
    Prereq {
        binary: "docker",
        label: "Docker Compose (plugin)",
        auto_installable: false,
        instructions: "Docker Compose v2 is bundled with Docker Desktop. For Linux: https://docs.docker.com/compose/install/",
    },
];

/// Prerequisite checker for the init wizard.
pub struct PrereqChecker {
    manual: bool,
    _need_ollama: bool,
}

impl PrereqChecker {
    pub fn new(manual: bool, need_ollama: bool) -> Self {
        Self {
            manual,
            _need_ollama: need_ollama,
        }
    }

    /// Check all prerequisites and, unless `manual` is set, attempt to install
    /// any that are missing. Returns an error if any required prerequisite is
    /// still missing after the install attempt.
    pub async fn check_and_install(&self) -> Result<()> {
        let prereqs: Vec<&Prereq> = PREREQS.iter().collect();

        let mut any_missing = false;

        for prereq in &prereqs {
            let status = check_binary(prereq.binary);
            match &status {
                PrereqStatus::Found => {
                    println!("  {} {}", "✓".green(), prereq.label);
                }
                PrereqStatus::Missing => {
                    println!("  {} {} not found", "✗".red(), prereq.label);

                    if self.manual {
                        println!("    → {}", prereq.instructions);
                        any_missing = true;
                    } else if prereq.auto_installable {
                        println!("    Attempting to install {}...", prereq.label);
                        match try_install(prereq.binary) {
                            Ok(()) => {
                                // Re-check after install
                                if check_binary(prereq.binary) == PrereqStatus::Found {
                                    println!(
                                        "    {} {} installed successfully",
                                        "✓".green(),
                                        prereq.label
                                    );
                                } else {
                                    println!(
                                        "    {} Install completed but {} still not found in PATH",
                                        "⚠".yellow(),
                                        prereq.binary
                                    );
                                    println!("    → {}", prereq.instructions);
                                    any_missing = true;
                                }
                            }
                            Err(e) => {
                                println!("    {} Auto-install failed: {}", "✗".red(), e);
                                println!("    → {}", prereq.instructions);
                                any_missing = true;
                            }
                        }
                    } else {
                        println!("    → {}", prereq.instructions);
                        any_missing = true;
                    }
                }
            }
        }

        // Special check: docker compose subcommand (v2 plugin)
        if check_binary("docker") == PrereqStatus::Found {
            let compose_ok = std::process::Command::new("docker")
                .args(["compose", "version"])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);

            if !compose_ok {
                println!("  {} Docker Compose v2 plugin not available", "✗".red());
                println!("    → https://docs.docker.com/compose/install/");
                any_missing = true;
            } else {
                println!("  {} Docker Compose v2", "✓".green());
            }
        }

        if any_missing {
            bail!(
                "One or more prerequisites are missing{}",
                if self.manual {
                    ". Install them manually and re-run `aegis init`."
                } else {
                    ". Re-run `aegis init --manual` for step-by-step instructions."
                }
            );
        }

        Ok(())
    }
}

fn check_binary(name: &str) -> PrereqStatus {
    match which::which(name) {
        Ok(_) => PrereqStatus::Found,
        Err(_) => PrereqStatus::Missing,
    }
}

/// Try to install a binary using the platform-native package manager.
fn try_install(binary: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        install_macos(binary)?;
    }
    #[cfg(target_os = "linux")]
    {
        install_linux(binary)?;
    }
    #[cfg(target_os = "windows")]
    {
        install_windows(binary)?;
    }
    #[allow(unreachable_code)]
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_macos(binary: &str) -> Result<()> {
    let pkg = match binary {
        "docker" => "docker",
        other => bail!("No brew formula known for '{}'", other),
    };
    let status = std::process::Command::new("brew")
        .args(["install", "--cask", pkg])
        .status()?;
    if !status.success() {
        bail!("brew install --cask {} exited with status {}", pkg, status);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_linux(binary: &str) -> Result<()> {
    match binary {
        "docker" => {
            // Use Docker's convenience script
            let status = std::process::Command::new("sh")
                .args(["-c", "curl -fsSL https://get.docker.com | sh"])
                .status()?;
            if !status.success() {
                bail!("Docker convenience install script exited with {}", status);
            }
        }
        other => bail!("No auto-install known for '{}' on Linux", other),
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn install_windows(binary: &str) -> Result<()> {
    let pkg = match binary {
        "docker" => "Docker.DockerDesktop",
        other => bail!("No winget package known for '{}'", other),
    };
    let status = std::process::Command::new("winget")
        .args(["install", "--id", pkg, "-e", "--silent"])
        .status()?;
    if !status.success() {
        bail!("winget install {} exited with {}", pkg, status);
    }
    Ok(())
}
