//! Unix service installation (systemd/launchd)

use anyhow::{Context, Result};
use colored::Colorize;
use std::path::PathBuf;
use tracing::info;

#[cfg(target_os = "linux")]
const SERVICE_TEMPLATE: &str = include_str!("../../templates/aegis.service");

#[cfg(target_os = "macos")]
const PLIST_TEMPLATE: &str = include_str!("../../templates/io.aegis.daemon.plist");

pub async fn install_service(
    binary_path: Option<PathBuf>,
    user: Option<String>,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        install_systemd(binary_path, user).await
    }

    #[cfg(target_os = "macos")]
    {
        install_launchd(binary_path, user).await
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        anyhow::bail!("Service installation only supported on Linux and macOS")
    }
}

pub async fn uninstall_service() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        uninstall_systemd().await
    }

    #[cfg(target_os = "macos")]
    {
        uninstall_launchd().await
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        anyhow::bail!("Service uninstallation only supported on Linux and macOS")
    }
}

#[cfg(target_os = "linux")]
async fn install_systemd(binary_path: Option<PathBuf>, user: Option<String>) -> Result<()> {
    use std::fs;

    info!("Installing systemd service");

    // Determine binary path
    let binary = binary_path
        .unwrap_or_else(|| std::env::current_exe().expect("Failed to get current exe"));

    // Verify binary exists
    if !binary.exists() {
        anyhow::bail!("Binary not found: {:?}", binary);
    }

    // Generate service file
    let service_content = SERVICE_TEMPLATE
        .replace("{{BINARY_PATH}}", &binary.display().to_string())
        .replace("{{USER}}", &user.unwrap_or_else(|| "root".to_string()));

    // Write to /etc/systemd/system/aegis.service
    let service_path = "/etc/systemd/system/aegis.service";

    fs::write(service_path, service_content)
        .with_context(|| format!("Failed to write service file: {}", service_path))?;

    println!(
        "{}",
        format!("✓ Service file created: {}", service_path).green()
    );

    // Reload systemd
    let output = std::process::Command::new("systemctl")
        .arg("daemon-reload")
        .output()
        .context("Failed to reload systemd")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to reload systemd: {}", stderr);
    }

    println!("{}", "✓ Systemd reloaded".green());

    println!();
    println!("{}", "Service installed successfully!".bold().green());
    println!();
    println!("To enable on boot:");
    println!("  sudo systemctl enable aegis");
    println!();
    println!("To start now:");
    println!("  sudo systemctl start aegis");
    println!();
    println!("To check status:");
    println!("  sudo systemctl status aegis");

    Ok(())
}

#[cfg(target_os = "linux")]
async fn uninstall_systemd() -> Result<()> {
    use std::fs;

    info!("Uninstalling systemd service");

    let service_path = "/etc/systemd/system/aegis.service";

    // Stop service if running
    let _ = std::process::Command::new("systemctl")
        .arg("stop")
        .arg("aegis")
        .output();

    // Disable service
    let _ = std::process::Command::new("systemctl")
        .arg("disable")
        .arg("aegis")
        .output();

    // Remove service file
    if std::path::Path::new(service_path).exists() {
        fs::remove_file(service_path)
            .with_context(|| format!("Failed to remove service file: {}", service_path))?;
        println!(
            "{}",
            format!("✓ Service file removed: {}", service_path).green()
        );
    }

    // Reload systemd
    let _ = std::process::Command::new("systemctl")
        .arg("daemon-reload")
        .output();

    println!("{}", "✓ Service uninstalled".green());

    Ok(())
}

#[cfg(target_os = "macos")]
async fn install_launchd(binary_path: Option<PathBuf>, _user: Option<String>) -> Result<()> {
    use std::fs;

    info!("Installing LaunchDaemon");

    // Determine binary path
    let binary = binary_path
        .unwrap_or_else(|| std::env::current_exe().expect("Failed to get current exe"));

    if !binary.exists() {
        anyhow::bail!("Binary not found: {:?}", binary);
    }

    // Generate plist file
    let plist_content = PLIST_TEMPLATE
        .replace("{{BINARY_PATH}}", &binary.display().to_string());

    // Write to /Library/LaunchDaemons/io.aegis.daemon.plist
    let plist_path = "/Library/LaunchDaemons/io.aegis.daemon.plist";

    fs::write(plist_path, plist_content)
        .with_context(|| format!("Failed to write plist file: {}", plist_path))?;

    println!(
        "{}",
        format!("✓ LaunchDaemon plist created: {}", plist_path).green()
    );

    // Load the LaunchDaemon
    let output = std::process::Command::new("launchctl")
        .arg("load")
        .arg(plist_path)
        .output()
        .context("Failed to load LaunchDaemon")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to load LaunchDaemon: {}", stderr);
    }

    println!("{}", "✓ LaunchDaemon loaded".green());

    println!();
    println!("{}", "Service installed successfully!".bold().green());
    println!();
    println!("The daemon will start automatically on boot.");
    println!();
    println!("To start now:");
    println!("  sudo launchctl start io.aegis.daemon");
    println!();
    println!("To check status:");
    println!("  sudo launchctl list | grep aegis");

    Ok(())
}

#[cfg(target_os = "macos")]
async fn uninstall_launchd() -> Result<()> {
    use std::fs;

    info!("Uninstalling LaunchDaemon");

    let plist_path = "/Library/LaunchDaemons/io.aegis.daemon.plist";

    // Unload the LaunchDaemon
    let _ = std::process::Command::new("launchctl")
        .arg("unload")
        .arg(plist_path)
        .output();

    // Remove plist file
    if std::path::Path::new(plist_path).exists() {
        fs::remove_file(plist_path)
            .with_context(|| format!("Failed to remove plist file: {}", plist_path))?;
        println!(
            "{}",
            format!("✓ LaunchDaemon plist removed: {}", plist_path).green()
        );
    }

    println!("{}", "✓ Service uninstalled".green());

    Ok(())
}
