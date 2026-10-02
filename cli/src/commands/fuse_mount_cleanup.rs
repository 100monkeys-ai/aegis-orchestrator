// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The FUSE daemon's start-up cleanup of what an earlier instance left under
//! its mount prefix (ADR-107).
//!
//! A FUSE mount outlives the process that served it: once that process has
//! exited, the kernel keeps the mount and answers every `stat(2)` of its mount
//! point with ENOTCONN ("Transport endpoint is not connected"). The cleanup
//! therefore finds mounts in the mount table, not by `stat`, and tells a dead
//! mount (ENOTCONN) from one another instance still serves (`stat` answers).
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Start-up cleanup of the host-side FUSE daemon's mount prefix

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use tracing::{info, warn};

/// How long the cleanup waits for a mount point's `stat(2)`: a mount whose
/// server is alive but wedged blocks it.
const STAT_TIMEOUT: Duration = Duration::from_secs(5);

/// What one unmount command did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandOutcome {
    /// The command line run.
    pub command: String,
    /// The exit code; `None` when the command could not be started or was
    /// ended by a signal.
    pub exit: Option<i32>,
    /// What it printed on standard error, trimmed.
    pub stderr: String,
}

impl CommandOutcome {
    fn succeeded(&self) -> bool {
        self.exit == Some(0)
    }
}

/// Every command, its exit and its standard error, on one line.
fn describe(outcomes: &[CommandOutcome]) -> String {
    outcomes
        .iter()
        .map(|o| {
            let exit = o
                .exit
                .map_or_else(|| "none".to_string(), |code| code.to_string());
            format!("`{}`: exit={} stderr={:?}", o.command, exit, o.stderr)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The host as the cleanup sees it.
pub(crate) trait MountSystem {
    /// `/proc/self/mountinfo`.
    fn mount_table(&self) -> io::Result<String>;
    /// Whether `path` is a directory, following it as `stat(2)` does; on a
    /// FUSE mount point this is answered by the mount's server.
    fn stat_is_dir(&self, path: &Path) -> io::Result<bool>;
    /// Lazily unmounts `path`: `fusermount -uz`, then `umount -l` when that
    /// fails. Every command run, in order.
    fn unmount(&self, path: &Path) -> Vec<CommandOutcome>;
}

/// The real host.
pub(crate) struct HostMountSystem;

fn run(program: &str, args: &[&str]) -> CommandOutcome {
    let command = std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    match Command::new(program).args(args).output() {
        Ok(out) => CommandOutcome {
            command,
            exit: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        },
        Err(e) => CommandOutcome {
            command,
            exit: None,
            stderr: format!("could not start: {e}"),
        },
    }
}

impl MountSystem for HostMountSystem {
    fn mount_table(&self) -> io::Result<String> {
        std::fs::read_to_string("/proc/self/mountinfo")
    }

    fn stat_is_dir(&self, path: &Path) -> io::Result<bool> {
        // On its own thread, so a wedged server cannot hold the daemon's
        // start; such a thread is left blocked in the kernel.
        let (tx, rx) = mpsc::channel();
        let target = path.to_path_buf();
        std::thread::spawn(move || {
            let _ = tx.send(std::fs::metadata(&target).map(|m| m.is_dir()));
        });
        rx.recv_timeout(STAT_TIMEOUT).unwrap_or_else(|_| {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("stat did not answer within {}s", STAT_TIMEOUT.as_secs()),
            ))
        })
    }

    fn unmount(&self, path: &Path) -> Vec<CommandOutcome> {
        let path = path.to_string_lossy();
        let fusermount = run("fusermount", &["-uz", &path]);
        if fusermount.succeeded() {
            return vec![fusermount];
        }
        let umount = run("umount", &["-l", &path]);
        vec![fusermount, umount]
    }
}

/// What the cleanup did under the prefix.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CleanupReport {
    /// Mount points it unmounted.
    pub unmounted: Vec<PathBuf>,
    /// Directories it removed.
    pub removed: Vec<PathBuf>,
    /// Paths it left in place, each with why.
    pub left: Vec<(PathBuf, String)>,
}

/// One line of `/proc/self/mountinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MountEntry {
    mount_point: PathBuf,
    fstype: String,
    source: String,
}

/// Reverses the kernel's octal escapes (`\040` for a space, and so on).
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let digits = bytes.get(i + 1..i + 4).unwrap_or_default();
        if bytes[i] == b'\\'
            && digits.len() == 3
            && digits.iter().all(|b| (b'0'..=b'7').contains(b))
        {
            let value = digits
                .iter()
                .fold(0u32, |acc, b| acc * 8 + u32::from(b - b'0'));
            if let Ok(byte) = u8::try_from(value) {
                out.push(byte);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The mounts strictly under `prefix`, in mount order (a mount stacked on
/// another comes after it).
fn mounts_under(table: &str, prefix: &Path) -> Vec<MountEntry> {
    table
        .lines()
        .filter_map(|line| {
            let (mount, filesystem) = line.split_once(" - ")?;
            let mount_point = PathBuf::from(unescape(mount.split(' ').nth(4)?));
            let mut filesystem = filesystem.split(' ');
            let fstype = filesystem.next()?.to_string();
            let source = unescape(filesystem.next().unwrap_or(""));
            Some(MountEntry {
                mount_point,
                fstype,
                source,
            })
        })
        .filter(|m| m.mount_point.starts_with(prefix) && m.mount_point != prefix)
        .collect()
}

fn is_fuse(fstype: &str) -> bool {
    fstype == "fuse" || fstype.starts_with("fuse.")
}

/// ENOTCONN, or ECONNABORTED after an abort through fusectl: the mount's
/// server is gone.
fn is_dead(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOTCONN) | Some(libc::ECONNABORTED)
    )
}

/// Unmounts `mount` when it is a dead FUSE mount, logging what was done.
/// Returns why it was left in place otherwise.
fn cleanup_mount(
    mount: &MountEntry,
    system: &dyn MountSystem,
    report: &mut CleanupReport,
) -> Option<String> {
    let path = &mount.mount_point;
    let not_dead = |reason: String| {
        warn!(
            path = %path.display(),
            source = %mount.source,
            fstype = %mount.fstype,
            reason = %reason,
            "Left mount under the FUSE mount prefix in place: not a dead FUSE mount"
        );
        Some(reason)
    };
    if !is_fuse(&mount.fstype) {
        return not_dead(format!("not a FUSE mount (type {})", mount.fstype));
    }
    match system.stat_is_dir(path) {
        Ok(_) => not_dead("still served: stat answers".to_string()),
        Err(e) if !is_dead(&e) => not_dead(format!("stat failed: {e}")),
        Err(_) => {
            let outcomes = system.unmount(path);
            let commands = describe(&outcomes);
            if outcomes.iter().any(CommandOutcome::succeeded) {
                info!(
                    path = %path.display(),
                    source = %mount.source,
                    commands = %commands,
                    "Unmounted stale FUSE mount (its server had exited)"
                );
                if !report.unmounted.contains(path) {
                    report.unmounted.push(path.clone());
                }
                None
            } else {
                warn!(
                    path = %path.display(),
                    source = %mount.source,
                    commands = %commands,
                    "Could not unmount stale FUSE mount; left in place"
                );
                Some(format!("unmount failed: {commands}"))
            }
        }
    }
}

/// Unmounts every dead FUSE mount under `prefix` and removes every directory
/// left directly under it, logging each act and each path left in place.
///
/// A mount whose server still answers is another instance's and is left
/// mounted; so is a mount the unmount commands refuse, with their exits and
/// standard error in the log. Only mount points the mount table places under
/// `prefix` are unmounted, and only directories directly under it (never
/// through a symbolic link) are removed.
pub(crate) fn cleanup_stale_mounts(prefix: &Path, system: &dyn MountSystem) -> CleanupReport {
    let mut report = CleanupReport::default();
    // Mount points in the table are canonical paths.
    let prefix = std::fs::canonicalize(prefix).unwrap_or_else(|_| prefix.to_path_buf());
    info!("Cleaning up stale FUSE mounts in {}", prefix.display());

    let mounts = match system.mount_table() {
        Ok(table) => mounts_under(&table, &prefix),
        Err(e) => {
            warn!(
                error = %e,
                "Could not read the mount table; no stale FUSE mount unmounted"
            );
            Vec::new()
        }
    };

    // Paths left in place: nothing under them is unmounted or removed.
    let mut left: HashSet<PathBuf> = HashSet::new();
    // The top of each stack first.
    for mount in mounts.iter().rev() {
        let path = &mount.mount_point;
        if left.contains(path) {
            continue;
        }
        if let Some(reason) = cleanup_mount(mount, system, &mut report) {
            left.insert(path.clone());
            report.left.push((path.clone(), reason));
        }
    }

    // What is still mounted after the unmounts.
    let still_mounted: HashSet<PathBuf> = system
        .mount_table()
        .map(|table| {
            mounts_under(&table, &prefix)
                .into_iter()
                .map(|m| m.mount_point)
                .collect()
        })
        .unwrap_or_default();

    match std::fs::read_dir(&prefix) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                // The entry's own type from the directory listing: no stat of
                // the mount point, and a symbolic link is not followed.
                if matches!(entry.file_type(), Ok(t) if !t.is_dir()) {
                    continue;
                }
                if left
                    .iter()
                    .chain(still_mounted.iter())
                    .any(|m| m.starts_with(&path))
                {
                    continue;
                }
                match std::fs::remove_dir(&path) {
                    Ok(()) => {
                        info!(path = %path.display(), "Reaped stale FUSE mount directory");
                        report.removed.push(path);
                    }
                    Err(e) => {
                        warn!(
                            path = %path.display(),
                            error = %e,
                            "Could not remove FUSE mount directory"
                        );
                        report.left.push((path, e.to_string()));
                    }
                }
            }
        }
        Err(e) => warn!(
            prefix = %prefix.display(),
            error = %e,
            "Could not list the FUSE mount prefix"
        ),
    }

    info!(
        unmounted = report.unmounted.len(),
        removed = report.removed.len(),
        left = report.left.len(),
        "Stale FUSE mount cleanup complete"
    );
    report
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{
        cleanup_stale_mounts, CleanupReport, CommandOutcome, HostMountSystem, MountSystem,
    };
    use std::collections::HashSet;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    /// Every formatted log line into one buffer.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Runs the cleanup with its log captured.
    fn cleanup_logged(prefix: &Path, system: &dyn MountSystem) -> (CleanupReport, String) {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        let report =
            tracing::subscriber::with_default(subscriber, || cleanup_stale_mounts(prefix, system));
        let log = String::from_utf8_lossy(&captured.0.lock().unwrap()).to_string();
        (report, log)
    }

    /// A stand-in host: a mount table, the answer `stat(2)` gives on each
    /// mount point, and the unmount commands' outcomes.
    struct StandIn {
        /// Mount points in mount order, each with its source.
        mounts: Mutex<Vec<(PathBuf, String)>>,
        /// Mount points whose server is gone: `stat` fails with ENOTCONN.
        dead: HashSet<PathBuf>,
        /// Mount points the unmount commands fail on, with what they print.
        refuse: Vec<(PathBuf, Vec<CommandOutcome>)>,
        unmount_calls: Mutex<Vec<PathBuf>>,
    }

    impl StandIn {
        fn new(mounts: &[(&Path, &str)]) -> Self {
            Self {
                mounts: Mutex::new(
                    mounts
                        .iter()
                        .map(|(p, s)| (p.to_path_buf(), s.to_string()))
                        .collect(),
                ),
                dead: HashSet::new(),
                refuse: Vec::new(),
                unmount_calls: Mutex::new(Vec::new()),
            }
        }

        fn dead(mut self, path: &Path) -> Self {
            self.dead.insert(path.to_path_buf());
            self
        }

        fn refusing(mut self, path: &Path, outcomes: Vec<CommandOutcome>) -> Self {
            self.refuse.push((path.to_path_buf(), outcomes));
            self
        }

        fn is_mounted(&self, path: &Path) -> bool {
            self.mounts.lock().unwrap().iter().any(|(p, _)| p == path)
        }

        fn unmount_calls(&self) -> Vec<PathBuf> {
            self.unmount_calls.lock().unwrap().clone()
        }
    }

    /// Octal escapes as the kernel writes them in mountinfo.
    fn escape(path: &Path) -> String {
        path.to_string_lossy()
            .replace('\\', "\\134")
            .replace(' ', "\\040")
            .replace('\t', "\\011")
            .replace('\n', "\\012")
    }

    impl MountSystem for StandIn {
        fn mount_table(&self) -> io::Result<String> {
            let mut table = String::from(
                "22 1 0:21 / / rw,relatime shared:1 - ext4 /dev/sdc rw\n\
                 23 22 0:22 / /tmp rw,nosuid,nodev shared:5 - tmpfs tmpfs rw\n",
            );
            for (i, (path, source)) in self.mounts.lock().unwrap().iter().enumerate() {
                table.push_str(&format!(
                    "{} 23 0:{} / {} rw,nosuid,nodev,relatime shared:{} - fuse {} rw,user_id=1003,group_id=1003\n",
                    100 + i,
                    60 + i,
                    escape(path),
                    700 + i,
                    source
                ));
            }
            Ok(table)
        }

        fn stat_is_dir(&self, path: &Path) -> io::Result<bool> {
            let top = self
                .mounts
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(p, _)| p == path)
                .map(|(p, _)| p.clone());
            match top {
                Some(p) if self.dead.contains(&p) => {
                    Err(io::Error::from_raw_os_error(libc::ENOTCONN))
                }
                _ => std::fs::metadata(path).map(|m| m.is_dir()),
            }
        }

        fn unmount(&self, path: &Path) -> Vec<CommandOutcome> {
            self.unmount_calls.lock().unwrap().push(path.to_path_buf());
            if let Some((_, outcomes)) = self.refuse.iter().find(|(p, _)| p == path) {
                return outcomes.clone();
            }
            let mut mounts = self.mounts.lock().unwrap();
            if let Some(i) = mounts.iter().rposition(|(p, _)| p == path) {
                mounts.remove(i);
            }
            vec![CommandOutcome {
                command: format!("fusermount -uz {}", path.display()),
                exit: Some(0),
                stderr: String::new(),
            }]
        }
    }

    fn prefix() -> tempfile::TempDir {
        tempfile::tempdir().expect("temporary mount prefix")
    }

    /// Regression (production, nuclear-vm-1, 2026-10-02): two mounts whose
    /// daemon had exited stood under the prefix across three restarts of the
    /// daemon. `stat` of such a mount point fails with ENOTCONN, so the
    /// start-up cleanup, which acted only on an entry `stat` called a
    /// directory, passed them by in silence.
    #[test]
    fn a_dead_mount_under_the_prefix_is_unmounted_removed_and_logged() {
        let prefix = prefix();
        let dead = prefix.path().join("9c0f7a9e-ea81-4afd-abaf-35763a69776b");
        std::fs::create_dir(&dead).unwrap();
        let host =
            StandIn::new(&[(&dead, "aegis-fsal-9c0f7a9e-ea81-4afd-abaf-35763a69776b")]).dead(&dead);

        let (report, log) = cleanup_logged(prefix.path(), &host);

        assert_eq!(host.unmount_calls(), vec![dead.clone()]);
        assert!(!host.is_mounted(&dead), "the dead mount must be unmounted");
        assert!(!dead.exists(), "its directory must be removed");
        assert_eq!(report.unmounted, vec![dead.clone()]);
        assert_eq!(report.removed, vec![dead.clone()]);
        assert!(report.left.is_empty(), "{:?}", report.left);
        assert!(
            log.contains("Unmounted stale FUSE mount")
                && log.contains("aegis-fsal-9c0f7a9e-ea81-4afd-abaf-35763a69776b"),
            "{log}"
        );
        assert!(
            log.contains("Reaped stale FUSE mount directory")
                && log.contains(&*dead.to_string_lossy()),
            "{log}"
        );
    }

    /// A directory with nothing mounted on it is removed.
    #[test]
    fn a_plain_leftover_directory_is_removed_and_logged() {
        let prefix = prefix();
        let leftover = prefix.path().join("21065fc4-2436-455c-8c6c-aa11dff666f6");
        std::fs::create_dir(&leftover).unwrap();
        let host = StandIn::new(&[]);

        let (report, log) = cleanup_logged(prefix.path(), &host);

        assert!(!leftover.exists());
        assert_eq!(report.removed, vec![leftover.clone()]);
        assert!(report.unmounted.is_empty());
        assert!(
            log.contains("Reaped stale FUSE mount directory")
                && log.contains(&*leftover.to_string_lossy()),
            "{log}"
        );
    }

    /// A dead mount the unmount commands refuse stays where it is, and the log
    /// says which commands ran, how each exited and what each printed.
    #[test]
    fn a_mount_it_cannot_unmount_is_left_and_logged_with_exit_and_stderr() {
        let prefix = prefix();
        let stuck = prefix.path().join("aaaaaaaa-0000-0000-0000-000000000000");
        std::fs::create_dir(&stuck).unwrap();
        let fusermount_stderr = format!(
            "fusermount: entry for {} not found in /etc/mtab",
            stuck.display()
        );
        let umount_stderr = format!("umount: {}: must be superuser to unmount.", stuck.display());
        let host = StandIn::new(&[(&stuck, "aegis-fsal-aaaaaaaa-0000-0000-0000-000000000000")])
            .dead(&stuck)
            .refusing(
                &stuck,
                vec![
                    CommandOutcome {
                        command: format!("fusermount -uz {}", stuck.display()),
                        exit: Some(1),
                        stderr: fusermount_stderr.clone(),
                    },
                    CommandOutcome {
                        command: format!("umount -l {}", stuck.display()),
                        exit: Some(32),
                        stderr: umount_stderr.clone(),
                    },
                ],
            );

        let (report, log) = cleanup_logged(prefix.path(), &host);

        assert_eq!(host.unmount_calls(), vec![stuck.clone()]);
        assert!(host.is_mounted(&stuck));
        assert!(stuck.is_dir(), "its directory must be left in place");
        assert!(report.unmounted.is_empty() && report.removed.is_empty());
        assert_eq!(
            report
                .left
                .iter()
                .map(|(p, _)| p.clone())
                .collect::<Vec<_>>(),
            vec![stuck.clone()]
        );
        assert!(log.contains("Could not unmount stale FUSE mount"), "{log}");
        assert!(
            log.contains("exit=1") && log.contains(&fusermount_stderr),
            "{log}"
        );
        assert!(
            log.contains("exit=32") && log.contains(&umount_stderr),
            "{log}"
        );
        assert!(!log.contains("Reaped stale FUSE mount directory"), "{log}");
    }

    /// A mount whose server still answers belongs to a live instance and is
    /// left mounted.
    #[test]
    fn a_mount_still_served_is_left_untouched_and_logged() {
        let prefix = prefix();
        let live = prefix.path().join("bbbbbbbb-0000-0000-0000-000000000000");
        std::fs::create_dir(&live).unwrap();
        let host = StandIn::new(&[(&live, "aegis-fsal-bbbbbbbb-0000-0000-0000-000000000000")]);

        let (report, log) = cleanup_logged(prefix.path(), &host);

        assert!(
            host.unmount_calls().is_empty(),
            "{:?}",
            host.unmount_calls()
        );
        assert!(host.is_mounted(&live) && live.is_dir());
        assert!(report.unmounted.is_empty() && report.removed.is_empty());
        assert!(
            log.contains("still served") && log.contains(&*live.to_string_lossy()),
            "{log}"
        );
    }

    /// Nothing outside the prefix is unmounted or removed: not a mount beside
    /// it whose path shares its leading characters, not a mount a symbolic
    /// link inside it points to (`fusermount` resolves the link and would
    /// unmount the target).
    #[test]
    fn nothing_outside_the_prefix_is_touched() {
        let root = prefix();
        let prefix = root.path().join("aegis-fuse-mounts");
        let sibling = root.path().join("aegis-fuse-mounts-other/cccccccc");
        let outside = root.path().join("outside");
        for dir in [&prefix, &sibling, &outside] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::os::unix::fs::symlink(&outside, prefix.join("link")).unwrap();
        let host = StandIn::new(&[
            (&sibling, "aegis-fsal-sibling"),
            (&outside, "aegis-fsal-outside"),
        ])
        .dead(&sibling)
        .dead(&outside);

        let (report, _log) = cleanup_logged(&prefix, &host);

        assert!(
            host.unmount_calls().is_empty(),
            "{:?}",
            host.unmount_calls()
        );
        assert!(host.is_mounted(&sibling) && host.is_mounted(&outside));
        assert!(sibling.is_dir() && outside.is_dir());
        assert!(
            prefix.join("link").symlink_metadata().is_ok(),
            "the link stays"
        );
        assert!(report.unmounted.is_empty() && report.removed.is_empty());
    }

    /// Two mounts stacked on one mount point (as a second mount of the same
    /// source can be) are both unmounted before the directory is removed.
    #[test]
    fn stacked_dead_mounts_on_one_mount_point_are_all_unmounted() {
        let prefix = prefix();
        let dead = prefix.path().join("dddddddd-0000-0000-0000-000000000000");
        std::fs::create_dir(&dead).unwrap();
        let host =
            StandIn::new(&[(&dead, "aegis-fsal-dddd"), (&dead, "aegis-fsal-dddd")]).dead(&dead);

        let (report, _log) = cleanup_logged(prefix.path(), &host);

        assert_eq!(host.unmount_calls(), vec![dead.clone(), dead.clone()]);
        assert!(!host.is_mounted(&dead) && !dead.exists());
        assert_eq!(report.removed, vec![dead.clone()]);
    }

    /// The mount table's octal escapes are read back, and a mount beside the
    /// prefix whose path shares its leading characters is not under it.
    #[test]
    fn the_mount_table_is_read_unescaped_and_by_path_component() {
        let table = "22 1 0:21 / / rw shared:1 - ext4 /dev/sdc rw\n\
                     101 23 0:61 / /tmp/aegis-fuse-mounts/a\\040b rw shared:7 - fuse aegis-fsal-a rw\n\
                     102 23 0:62 / /tmp/aegis-fuse-mounts-x/c rw shared:8 - fuse aegis-fsal-c rw\n\
                     103 23 0:63 / /tmp/aegis-fuse-mounts rw shared:9 - tmpfs tmpfs rw\n";
        let mounts = super::mounts_under(table, Path::new("/tmp/aegis-fuse-mounts"));
        assert_eq!(
            mounts,
            vec![super::MountEntry {
                mount_point: PathBuf::from("/tmp/aegis-fuse-mounts/a b"),
                fstype: "fuse".to_string(),
                source: "aegis-fsal-a".to_string(),
            }]
        );
    }

    // The tests below make real FUSE mounts with `fuse-overlayfs` and need
    // `/dev/fuse`; run them with `--ignored` on a host that has both.

    pub(crate) struct Overlay {
        pub(crate) mountpoint: PathBuf,
        server: std::process::Child,
    }

    /// Mounts a `fuse-overlayfs` at `mountpoint`, backed by `scratch`.
    pub(crate) fn overlay(scratch: &Path, mountpoint: &Path) -> Overlay {
        let name = mountpoint
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let (lower, upper, work) = (
            scratch.join(format!("{name}-lower")),
            scratch.join(format!("{name}-upper")),
            scratch.join(format!("{name}-work")),
        );
        for dir in [&lower, &upper, &work, &mountpoint.to_path_buf()] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(lower.join("file"), "x").unwrap();
        let options = format!(
            "lowerdir={},upperdir={},workdir={}",
            lower.display(),
            upper.display(),
            work.display()
        );
        let server = std::process::Command::new("fuse-overlayfs")
            .args(["-f", "-o", &options])
            .arg(mountpoint)
            .spawn()
            .expect("fuse-overlayfs");
        // Dropped (and so killed and reaped) if the mount never appears.
        let overlay = Overlay {
            mountpoint: mountpoint.to_path_buf(),
            server,
        };
        for _ in 0..100 {
            if mounted(mountpoint) {
                return overlay;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("fuse-overlayfs did not mount {}", mountpoint.display());
    }

    impl Overlay {
        /// Kills the server: the mount stays and answers ENOTCONN.
        pub(crate) fn kill(&mut self) {
            self.server.kill().expect("kill fuse-overlayfs");
            self.server.wait().expect("reap fuse-overlayfs");
            for _ in 0..100 {
                if matches!(std::fs::metadata(&self.mountpoint), Err(e) if e.raw_os_error() == Some(libc::ENOTCONN))
                {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            panic!("{} did not go dead", self.mountpoint.display());
        }
    }

    impl Drop for Overlay {
        fn drop(&mut self) {
            if mounted(&self.mountpoint) {
                let _ = std::process::Command::new("fusermount")
                    .arg("-uz")
                    .arg(&self.mountpoint)
                    .output();
            }
            let _ = self.server.kill();
            let _ = self.server.wait();
        }
    }

    pub(crate) fn mounted(path: &Path) -> bool {
        let table = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
        let escaped = escape(path);
        table
            .lines()
            .any(|l| l.split(' ').nth(4) == Some(escaped.as_str()))
    }

    /// The production case on a real mount: a FUSE mount whose server was
    /// killed is unmounted and its directory removed.
    #[test]
    #[ignore = "needs fuse-overlayfs and /dev/fuse"]
    fn real_dead_fuse_mount_is_unmounted_and_removed() {
        let root = prefix();
        let prefix = root.path().join("mounts");
        let mut dead = overlay(
            root.path(),
            &prefix.join("9c0f7a9e-ea81-4afd-abaf-35763a69776b"),
        );
        dead.kill();

        let (report, log) = cleanup_logged(&prefix, &HostMountSystem);

        assert!(!mounted(&dead.mountpoint), "{log}");
        assert!(!dead.mountpoint.exists(), "{log}");
        assert_eq!(report.unmounted, vec![dead.mountpoint.clone()], "{log}");
    }

    /// A real FUSE mount whose server is alive is left mounted.
    #[test]
    #[ignore = "needs fuse-overlayfs and /dev/fuse"]
    fn real_live_fuse_mount_is_left_mounted() {
        let root = prefix();
        let prefix = root.path().join("mounts");
        let live = overlay(
            root.path(),
            &prefix.join("bbbbbbbb-0000-0000-0000-000000000000"),
        );

        let (report, log) = cleanup_logged(&prefix, &HostMountSystem);

        assert!(mounted(&live.mountpoint), "{log}");
        assert!(live.mountpoint.join("file").is_file(), "{log}");
        assert!(
            report.unmounted.is_empty() && report.removed.is_empty(),
            "{log}"
        );
    }

    /// A real live FUSE mount outside the prefix, reached through a symbolic
    /// link inside it, is left mounted.
    #[test]
    #[ignore = "needs fuse-overlayfs and /dev/fuse"]
    fn real_fuse_mount_behind_a_link_in_the_prefix_is_left_mounted() {
        let root = prefix();
        let prefix = root.path().join("mounts");
        std::fs::create_dir_all(&prefix).unwrap();
        let outside = overlay(root.path(), &root.path().join("outside"));
        std::os::unix::fs::symlink(&outside.mountpoint, prefix.join("link")).unwrap();

        let (_report, log) = cleanup_logged(&prefix, &HostMountSystem);

        assert!(mounted(&outside.mountpoint), "{log}");
        assert!(prefix.join("link").symlink_metadata().is_ok(), "{log}");
    }
}
