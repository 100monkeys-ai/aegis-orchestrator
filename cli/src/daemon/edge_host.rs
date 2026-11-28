// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Host state on an edge daemon (ADR-157 D2, D3): `host.facts` and
//! `host.apply`.
//!
//! A state is an ordered list of resources. Each resource reads the host
//! first and acts only when it differs, so applying a state twice changes
//! nothing the second time. In check mode every resource reports what it
//! would do and does nothing. Every resource reports `ok`, `changed` or
//! `failed`, with a short diff.
//!
//! Resource kinds: `file`, `directory`, `package`, `service`, `user`,
//! `group`, `sysctl`, `firewall`, `command`, `pod`.

use std::collections::HashSet;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::process::Command;

/// Where the daemon keeps what it needs between applies (pod fingerprints).
pub const HOST_STATE_DIR: &str = "/var/lib/aegis-edge";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Changed,
    Failed,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Changed => "changed",
            Outcome::Failed => "failed",
        }
    }
}

struct Report {
    outcome: Outcome,
    diff: String,
}

fn ok(diff: impl Into<String>) -> Report {
    Report {
        outcome: Outcome::Ok,
        diff: diff.into(),
    }
}
fn changed(diff: impl Into<String>) -> Report {
    Report {
        outcome: Outcome::Changed,
        diff: diff.into(),
    }
}
fn failed(diff: impl Into<String>) -> Report {
    Report {
        outcome: Outcome::Failed,
        diff: diff.into(),
    }
}

/// Run a command, answering (success, stdout, stderr).
async fn run(argv: &[String]) -> (bool, String, String) {
    let Some((prog, rest)) = argv.split_first() else {
        return (false, String::new(), "empty command".into());
    };
    match Command::new(prog)
        .args(rest)
        .env("DEBIAN_FRONTEND", "noninteractive")
        .stdin(std::process::Stdio::null())
        .output()
        .await
    {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        ),
        Err(e) => (false, String::new(), format!("spawn {prog}: {e}")),
    }
}

fn argv(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::Array(a) => a.iter().map(|x| x.as_str().map(str::to_string)).collect(),
        Value::String(s) => Some(vec!["sh".into(), "-c".into(), s.clone()]),
        _ => None,
    }
}

fn s<'a>(r: &'a Value, k: &str) -> Option<&'a str> {
    r.get(k).and_then(Value::as_str)
}

fn want_present(r: &Value) -> bool {
    s(r, "state").map(|st| st != "absent").unwrap_or(true)
}

fn mode_of(r: &Value) -> Result<Option<u32>, String> {
    match r.get("mode") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(m)) => u32::from_str_radix(m.trim_start_matches("0o"), 8)
            .map(Some)
            .map_err(|_| format!("mode {m:?} is not octal")),
        Some(v) => Err(format!("mode {v} must be an octal string")),
    }
}

async fn uid_of(user: &str) -> Option<u32> {
    let (okay, out, _) = run(&["id".into(), "-u".into(), user.into()]).await;
    okay.then(|| out.trim().parse().ok()).flatten()
}

async fn gid_of(group: &str) -> Option<u32> {
    let (okay, out, _) = run(&["getent".into(), "group".into(), group.into()]).await;
    if !okay {
        return None;
    }
    out.split(':').nth(2).and_then(|g| g.trim().parse().ok())
}

/// Owner, group and mode of `path` against the resource; the changes
/// needed, applied unless `check`.
async fn ensure_attrs(path: &Path, r: &Value, check: bool) -> Result<Vec<String>, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    let mut diffs = Vec::new();
    if let Some(mode) = mode_of(r)? {
        let have = meta.permissions().mode() & 0o7777;
        if have != mode {
            diffs.push(format!("mode {have:o} -> {mode:o}"));
            if !check {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                    .map_err(|e| format!("chmod: {e}"))?;
            }
        }
    }
    let uid = match s(r, "owner") {
        Some(o) => Some(uid_of(o).await.ok_or_else(|| format!("no user {o}"))?),
        None => None,
    };
    let gid = match s(r, "group") {
        Some(g) => Some(gid_of(g).await.ok_or_else(|| format!("no group {g}"))?),
        None => None,
    };
    if uid.is_some_and(|u| u != meta.uid()) || gid.is_some_and(|g| g != meta.gid()) {
        diffs.push(format!(
            "owner {}:{} -> {}:{}",
            meta.uid(),
            meta.gid(),
            uid.unwrap_or(meta.uid()),
            gid.unwrap_or(meta.gid())
        ));
        if !check {
            std::os::unix::fs::chown(path, uid, gid).map_err(|e| format!("chown: {e}"))?;
        }
    }
    Ok(diffs)
}

async fn file(r: &Value, check: bool) -> Report {
    let Some(path) = s(r, "path").map(PathBuf::from) else {
        return failed("file needs path");
    };
    if !want_present(r) {
        if !path.exists() {
            return ok("absent");
        }
        if !check {
            if let Err(e) = std::fs::remove_file(&path) {
                return failed(format!("remove: {e}"));
            }
        }
        return changed("removed");
    }
    let Some(content) = s(r, "content") else {
        return failed("file needs content (or state: absent)");
    };
    let mut diffs = Vec::new();
    let have = std::fs::read(&path).ok();
    if have.as_deref() != Some(content.as_bytes()) {
        diffs.push(match &have {
            None => format!("created ({} bytes)", content.len()),
            Some(h) => format!("content {} -> {} bytes", h.len(), content.len()),
        });
        if !check {
            if let Some(dir) = path.parent() {
                if let Err(e) = std::fs::create_dir_all(dir) {
                    return failed(format!("mkdir {}: {e}", dir.display()));
                }
            }
            let tmp = path.with_extension("aegis-edge-tmp");
            if let Err(e) = std::fs::write(&tmp, content).and_then(|_| std::fs::rename(&tmp, &path))
            {
                return failed(format!("write: {e}"));
            }
        }
    }
    if path.exists() {
        match ensure_attrs(&path, r, check).await {
            Ok(mut d) => diffs.append(&mut d),
            Err(e) => return failed(e),
        }
    }
    if diffs.is_empty() {
        ok("in place")
    } else {
        changed(diffs.join("; "))
    }
}

async fn directory(r: &Value, check: bool) -> Report {
    let Some(path) = s(r, "path").map(PathBuf::from) else {
        return failed("directory needs path");
    };
    let mut diffs = Vec::new();
    if !path.is_dir() {
        diffs.push("created".to_string());
        if !check {
            if let Err(e) = std::fs::create_dir_all(&path) {
                return failed(format!("mkdir: {e}"));
            }
        }
    }
    if path.is_dir() {
        match ensure_attrs(&path, r, check).await {
            Ok(mut d) => diffs.append(&mut d),
            Err(e) => return failed(e),
        }
    }
    if diffs.is_empty() {
        ok("in place")
    } else {
        changed(diffs.join("; "))
    }
}

async fn package(r: &Value, check: bool) -> Report {
    let Some(name) = s(r, "name") else {
        return failed("package needs name");
    };
    let (_, out, _) = run(&[
        "dpkg-query".into(),
        "-W".into(),
        "-f=${Status}".into(),
        name.into(),
    ])
    .await;
    let installed = out.contains("install ok installed");
    let want = want_present(r);
    if installed == want {
        return ok(if want { "installed" } else { "absent" });
    }
    if check {
        return changed(if want {
            "would install"
        } else {
            "would remove"
        });
    }
    let cmd: Vec<String> = if want {
        vec![
            "apt-get".into(),
            "install".into(),
            "-y".into(),
            "--no-install-recommends".into(),
            name.into(),
        ]
    } else {
        vec!["apt-get".into(), "remove".into(), "-y".into(), name.into()]
    };
    let (okay, _, err) = run(&cmd).await;
    if okay {
        changed(if want { "installed" } else { "removed" })
    } else {
        failed(err.lines().last().unwrap_or("apt-get failed").to_string())
    }
}

async fn service(r: &Value, check: bool, restart: bool) -> Report {
    let Some(name) = s(r, "name") else {
        return failed("service needs name");
    };
    let mut diffs = Vec::new();
    if let Some(want) = r.get("enabled").and_then(Value::as_bool) {
        let (on, _, _) = run(&[
            "systemctl".into(),
            "is-enabled".into(),
            "--quiet".into(),
            name.into(),
        ])
        .await;
        if on != want {
            diffs.push(format!("enabled {on} -> {want}"));
            if !check {
                let verb = if want { "enable" } else { "disable" };
                let (okay, _, err) = run(&["systemctl".into(), verb.into(), name.into()]).await;
                if !okay {
                    return failed(err);
                }
            }
        }
    }
    let (active, _, _) = run(&[
        "systemctl".into(),
        "is-active".into(),
        "--quiet".into(),
        name.into(),
    ])
    .await;
    let want_running = r.get("running").and_then(Value::as_bool);
    if let Some(want) = want_running {
        if active != want {
            diffs.push(format!("running {active} -> {want}"));
            if !check {
                let verb = if want { "start" } else { "stop" };
                let (okay, _, err) = run(&["systemctl".into(), verb.into(), name.into()]).await;
                if !okay {
                    return failed(err);
                }
            }
        }
    }
    if restart && active && want_running != Some(false) {
        diffs.push("restarted: a watched resource changed".into());
        if !check {
            // A watched resource may be the unit file itself, or a drop-in:
            // systemd reads it again only after a reload.
            let (okay, _, err) = run(&["systemctl".into(), "daemon-reload".into()]).await;
            if !okay {
                return failed(err);
            }
            let (okay, _, err) = run(&["systemctl".into(), "restart".into(), name.into()]).await;
            if !okay {
                return failed(err);
            }
        }
    }
    if diffs.is_empty() {
        ok("in place")
    } else {
        changed(diffs.join("; "))
    }
}

async fn group(r: &Value, check: bool) -> Report {
    let Some(name) = s(r, "name") else {
        return failed("group needs name");
    };
    if gid_of(name).await.is_some() {
        return ok("exists");
    }
    if check {
        return changed("would create");
    }
    let mut cmd: Vec<String> = vec!["groupadd".into()];
    if let Some(gid) = r.get("gid").and_then(Value::as_u64) {
        cmd.extend(["--gid".into(), gid.to_string()]);
    }
    cmd.push(name.into());
    let (okay, _, err) = run(&cmd).await;
    if okay {
        changed("created")
    } else {
        failed(err)
    }
}

async fn user(r: &Value, check: bool) -> Report {
    let Some(name) = s(r, "name") else {
        return failed("user needs name");
    };
    let mut diffs = Vec::new();
    if uid_of(name).await.is_none() {
        diffs.push("created".to_string());
        if !check {
            let mut cmd: Vec<String> = vec!["useradd".into(), "--create-home".into()];
            if let Some(uid) = r.get("uid").and_then(Value::as_u64) {
                cmd.extend(["--uid".into(), uid.to_string()]);
            }
            if let Some(shell) = s(r, "shell") {
                cmd.extend(["--shell".into(), shell.into()]);
            }
            cmd.push(name.into());
            let (okay, _, err) = run(&cmd).await;
            if !okay {
                return failed(err);
            }
        }
    }
    if let Some(groups) = r.get("groups").and_then(Value::as_array) {
        let (_, out, _) = run(&["id".into(), "-nG".into(), name.into()]).await;
        let have: HashSet<&str> = out.split_whitespace().collect();
        for g in groups.iter().filter_map(Value::as_str) {
            if !have.contains(g) {
                diffs.push(format!("joined {g}"));
                if !check {
                    let (okay, _, err) =
                        run(&["usermod".into(), "-aG".into(), g.into(), name.into()]).await;
                    if !okay {
                        return failed(err);
                    }
                }
            }
        }
    }
    if r.get("linger").and_then(Value::as_bool) == Some(true)
        && !Path::new("/var/lib/systemd/linger").join(name).exists()
    {
        diffs.push("linger enabled".into());
        if !check {
            let (okay, _, err) =
                run(&["loginctl".into(), "enable-linger".into(), name.into()]).await;
            if !okay {
                return failed(err);
            }
        }
    }
    if diffs.is_empty() {
        ok("in place")
    } else {
        changed(diffs.join("; "))
    }
}

async fn sysctl(r: &Value, check: bool) -> Report {
    let (Some(key), Some(value)) = (s(r, "key"), r.get("value")) else {
        return failed("sysctl needs key and value");
    };
    let value = match value {
        Value::String(v) => v.clone(),
        other => other.to_string(),
    };
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        return failed(format!("sysctl key {key:?} is not a key"));
    }
    let live = std::fs::read_to_string(Path::new("/proc/sys").join(key.replace('.', "/")))
        .map(|v| v.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let conf = PathBuf::from(format!("/etc/sysctl.d/90-aegis-{key}.conf"));
    let line = format!("{key} = {value}\n");
    let persisted = std::fs::read_to_string(&conf).ok().as_deref() == Some(line.as_str());
    let mut diffs = Vec::new();
    if live != value {
        diffs.push(format!("{key} {live} -> {value}"));
    }
    if !persisted {
        diffs.push(format!("persisted in {}", conf.display()));
    }
    if diffs.is_empty() {
        return ok("in place");
    }
    if !check {
        if let Err(e) = std::fs::write(&conf, &line) {
            return failed(format!("write {}: {e}", conf.display()));
        }
        let (okay, _, err) = run(&["sysctl".into(), "-w".into(), format!("{key}={value}")]).await;
        if !okay {
            return failed(err);
        }
    }
    changed(diffs.join("; "))
}

async fn firewall(r: &Value, check: bool) -> Report {
    let Some(rule) = s(r, "rule") else {
        return failed("firewall needs rule, e.g. \"allow 443/tcp\"");
    };
    let (_, added, _) = run(&["ufw".into(), "show".into(), "added".into()]).await;
    let present = added.lines().any(|l| l.trim() == format!("ufw {rule}"));
    let want = want_present(r);
    if present == want {
        return ok(if want { "present" } else { "absent" });
    }
    if check {
        return changed(if want { "would add" } else { "would delete" });
    }
    let mut cmd: Vec<String> = vec!["ufw".into()];
    if !want {
        cmd.push("delete".into());
    }
    cmd.extend(rule.split_whitespace().map(str::to_string));
    let (okay, _, err) = run(&cmd).await;
    if okay {
        changed(if want { "added" } else { "deleted" })
    } else {
        failed(err)
    }
}

async fn command(r: &Value, check: bool) -> Report {
    let Some(cmd) = r.get("run").and_then(argv) else {
        return failed("command needs run");
    };
    if let Some(path) = s(r, "creates") {
        if Path::new(path).exists() {
            return ok(format!("{path} exists"));
        }
    }
    if let Some(guard) = r.get("unless").and_then(argv) {
        if run(&guard).await.0 {
            return ok("unless guard held");
        }
    }
    if let Some(guard) = r.get("onlyif").and_then(argv) {
        if !run(&guard).await.0 {
            return ok("onlyif guard did not hold");
        }
    }
    if r.get("creates").is_none() && r.get("unless").is_none() && r.get("onlyif").is_none() {
        return failed("command needs creates, unless or onlyif, so it is idempotent");
    }
    if check {
        return changed(format!("would run {}", cmd.join(" ")));
    }
    let (okay, _, err) = run(&cmd).await;
    if okay {
        changed("ran")
    } else {
        failed(err.lines().last().unwrap_or("failed").to_string())
    }
}

/// A Podman kube play, applied as `user` when the rendered pod changed.
async fn pod(r: &Value, check: bool) -> Report {
    let (Some(name), Some(spec), Some(as_user)) = (s(r, "name"), s(r, "spec"), s(r, "user")) else {
        return failed("pod needs name, spec and user");
    };
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return failed(format!("pod name {name:?} is not a name"));
    }
    let Some(uid) = uid_of(as_user).await else {
        return failed(format!("no user {as_user}"));
    };
    let fingerprint = hex::encode(Sha256::digest(spec.as_bytes()));
    let dir = Path::new(HOST_STATE_DIR).join("pods");
    let marker = dir.join(format!("{name}.sha256"));
    let running = run(&[
        "runuser".into(),
        "-u".into(),
        as_user.into(),
        "--".into(),
        "env".into(),
        format!("XDG_RUNTIME_DIR=/run/user/{uid}"),
        "podman".into(),
        "pod".into(),
        "exists".into(),
        name.into(),
    ])
    .await
    .0;
    if running && std::fs::read_to_string(&marker).ok().as_deref() == Some(fingerprint.as_str()) {
        return ok("pod unchanged");
    }
    if check {
        return changed(if running {
            "would replace (spec changed)"
        } else {
            "would start"
        });
    }
    let spec_path = dir.join(format!("{name}.yaml"));
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&spec_path, spec)) {
        return failed(format!("write pod spec: {e}"));
    }
    let _ = std::os::unix::fs::chown(&spec_path, Some(uid), None);
    let (okay, _, err) = run(&[
        "runuser".into(),
        "-u".into(),
        as_user.into(),
        "--".into(),
        "env".into(),
        format!("XDG_RUNTIME_DIR=/run/user/{uid}"),
        "podman".into(),
        "kube".into(),
        "play".into(),
        "--replace".into(),
        spec_path.display().to_string(),
    ])
    .await;
    if !okay {
        return failed(err.lines().last().unwrap_or("kube play failed").to_string());
    }
    if let Err(e) = std::fs::write(&marker, &fingerprint) {
        return failed(format!("record fingerprint: {e}"));
    }
    changed(if running { "replaced" } else { "started" })
}

/// Apply a state: `{"resources": [ {"id", "kind", ...} ]}`. Resources run in
/// order; a `service` with `watch: [ids]` restarts when any of them changed.
/// The first failure stops the run unless `continue_on_failure` is set.
pub async fn apply(state: &Value, check: bool) -> Value {
    let mut resources = state
        .get("resources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // A state may name this host's facts, ((facts.interfaces.wg0|127.0.0.1)):
    // rendered here, where the facts are, before anything is applied. A
    // fact the host lacks, with no default, refuses the whole state.
    if resources.iter().any(names_a_fact) {
        let known = facts().await;
        match render_facts(&Value::Array(resources), &known) {
            Ok(Value::Array(rendered)) => resources = rendered,
            Ok(_) => unreachable!("an array renders to an array"),
            Err(missing) => {
                return json!({
                    "changed": 0,
                    "failed": 1,
                    "results": [{ "id": "facts", "status": "failed", "diff": missing }],
                })
            }
        }
    }
    let keep_going = state
        .get("continue_on_failure")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut changed_ids: HashSet<String> = HashSet::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    let (mut n_changed, mut n_failed) = (0, 0);
    for (i, r) in resources.iter().enumerate() {
        let id = s(r, "id")
            .map(str::to_string)
            .unwrap_or_else(|| format!("#{i}"));
        let kind = s(r, "kind").unwrap_or("");
        let report = if !seen.insert(id.clone()) {
            failed(format!("duplicate id {id}"))
        } else {
            match kind {
                "file" => file(r, check).await,
                "directory" => directory(r, check).await,
                "package" => package(r, check).await,
                "service" => {
                    let restart = r.get("watch").and_then(Value::as_array).is_some_and(|w| {
                        w.iter()
                            .filter_map(Value::as_str)
                            .any(|x| changed_ids.contains(x))
                    });
                    service(r, check, restart).await
                }
                "user" => user(r, check).await,
                "group" => group(r, check).await,
                "sysctl" => sysctl(r, check).await,
                "firewall" => firewall(r, check).await,
                "command" => command(r, check).await,
                "pod" => pod(r, check).await,
                other => failed(format!("unknown kind {other:?}")),
            }
        };
        match report.outcome {
            Outcome::Changed => {
                n_changed += 1;
                changed_ids.insert(id.clone());
            }
            Outcome::Failed => n_failed += 1,
            Outcome::Ok => {}
        }
        out.push(json!({ "id": id, "kind": kind, "status": report.outcome.as_str(), "diff": report.diff }));
        if report.outcome == Outcome::Failed && !keep_going {
            break;
        }
    }
    json!({
        "check": check,
        "changed": n_changed,
        "failed": n_failed,
        "resources": out,
    })
}

/// What the host is: the facts a state's templates and a workflow's
/// targeting read.
fn names_a_fact(v: &Value) -> bool {
    match v {
        Value::String(s) => s.contains("((facts."),
        Value::Array(a) => a.iter().any(names_a_fact),
        Value::Object(o) => o.values().any(names_a_fact),
        _ => false,
    }
}

/// Replace every `((facts.PATH))` or `((facts.PATH|DEFAULT))` with the fact
/// at PATH (dot-separated keys into `facts`), or DEFAULT where the host has
/// none. Err names every fact the host lacks with no default.
fn render_facts(v: &Value, facts: &Value) -> Result<Value, String> {
    fn lookup(facts: &Value, path: &str) -> Option<String> {
        let mut at = facts;
        for key in path.split('.') {
            at = match at {
                Value::Object(o) => o.get(key)?,
                Value::Array(a) => a.get(key.parse::<usize>().ok()?)?,
                _ => return None,
            };
        }
        match at {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }
    fn walk(v: &Value, facts: &Value, missing: &mut Vec<String>) -> Value {
        match v {
            Value::String(s) => {
                let mut out = String::with_capacity(s.len());
                let mut rest = s.as_str();
                while let Some(open) = rest.find("((facts.") {
                    let Some(close) = rest[open..].find("))").map(|i| i + open) else {
                        break;
                    };
                    out.push_str(&rest[..open]);
                    let inner = rest[open + 8..close].trim();
                    let (path, default) = match inner.split_once('|') {
                        Some((p, d)) => (p.trim(), Some(d.trim())),
                        None => (inner, None),
                    };
                    match lookup(facts, path).or_else(|| default.map(str::to_string)) {
                        Some(value) => out.push_str(&value),
                        None => {
                            missing.push(format!("facts.{path}"));
                            out.push_str(&rest[open..close + 2]);
                        }
                    }
                    rest = &rest[close + 2..];
                }
                out.push_str(rest);
                Value::String(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(|x| walk(x, facts, missing)).collect()),
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, x)| (k.clone(), walk(x, facts, missing)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    let mut missing = Vec::new();
    let rendered = walk(v, facts, &mut missing);
    if missing.is_empty() {
        Ok(rendered)
    } else {
        missing.dedup();
        Err(format!(
            "this host has no {} and the state gives no default; nothing was applied",
            missing.join(", ")
        ))
    }
}

/// Each interface's first IPv4 address, from `ip -4 -o addr show`.
fn interfaces_from(ip_output: &str) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    for line in ip_output.lines() {
        let mut words = line.split_whitespace();
        let (Some(_), Some(name)) = (words.next(), words.next()) else {
            continue;
        };
        let addr = words
            .skip_while(|w| *w != "inet")
            .nth(1)
            .and_then(|cidr| cidr.split('/').next());
        if let Some(addr) = addr {
            out.entry(name.trim_end_matches(':').to_string())
                .or_insert_with(|| Value::String(addr.to_string()));
        }
    }
    out
}

pub async fn facts() -> Value {
    let os = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let field = |k: &str| {
        os.lines()
            .find_map(|l| l.strip_prefix(&format!("{k}=")))
            .map(|v| v.trim_matches('"').to_string())
    };
    let mem_kb = std::fs::read_to_string("/proc/meminfo").ok().and_then(|m| {
        m.lines()
            .find_map(|l| l.strip_prefix("MemTotal:").map(|v| v.trim().to_string()))
    });
    let (_, kernel, _) = run(&["uname".into(), "-r".into()]).await;
    let (_, addrs, _) = run(&["hostname".into(), "-I".into()]).await;
    let (_, ip_out, _) = run(&[
        "ip".into(),
        "-4".into(),
        "-o".into(),
        "addr".into(),
        "show".into(),
    ])
    .await;
    json!({
        "hostname": std::fs::read_to_string("/etc/hostname").unwrap_or_default().trim(),
        "os_id": field("ID"),
        "os_version": field("VERSION_ID"),
        "kernel": kernel.trim(),
        "arch": std::env::consts::ARCH,
        "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "mem_total": mem_kb,
        "addresses": addrs.split_whitespace().collect::<Vec<_>>(),
        "interfaces": interfaces_from(&ip_out),
        "systemd": Path::new("/run/systemd/system").exists(),
        "podman": run(&["podman".into(), "--version".into()]).await.0,
        "ufw": Path::new("/usr/sbin/ufw").exists(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interfaces_are_read_from_ip_addr() {
        let out = "1: lo    inet 127.0.0.1/8 scope host lo\n2: ens3    inet 135.148.170.197/32 metric 100 scope global dynamic ens3\n5: wg0    inet 10.121.0.2/24 scope global wg0\n";
        let i = interfaces_from(out);
        assert_eq!(i["wg0"], "10.121.0.2");
        assert_eq!(i["ens3"], "135.148.170.197");
    }

    #[test]
    fn facts_render_with_defaults_and_refuse_when_missing() {
        let facts = json!({ "hostname": "aegis-node-1", "interfaces": { "wg0": "10.121.0.2" } });
        let state =
            json!([{ "content": "ip=((facts.interfaces.wg0|127.0.0.1)) on ((facts.hostname))" }]);
        assert_eq!(
            render_facts(&state, &facts).unwrap()[0]["content"],
            "ip=10.121.0.2 on aegis-node-1"
        );
        let standalone = json!({ "hostname": "nuclear-vm-1", "interfaces": {} });
        assert_eq!(
            render_facts(&state, &standalone).unwrap()[0]["content"],
            "ip=127.0.0.1 on nuclear-vm-1"
        );
        let err = render_facts(&json!(["((facts.interfaces.wg9))"]), &facts).unwrap_err();
        assert!(err.contains("facts.interfaces.wg9"), "{err}");
    }

    #[tokio::test]
    async fn check_mode_reports_and_changes_nothing_then_apply_converges() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("etc/app.conf");
        let state = json!({ "resources": [
            { "id": "conf-dir", "kind": "directory", "path": dir.path().join("etc"), "mode": "0750" },
            { "id": "conf", "kind": "file", "path": target, "content": "a = 1\n", "mode": "0640" },
            { "id": "seed", "kind": "command", "run": ["touch", dir.path().join("seeded")], "creates": dir.path().join("seeded") },
        ]});

        let dry = apply(&state, true).await;
        assert_eq!(dry["changed"], 3, "{dry}");
        assert!(!target.exists(), "check mode wrote nothing");
        assert!(!dir.path().join("seeded").exists());

        let first = apply(&state, false).await;
        assert_eq!(first["changed"], 3, "{first}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "a = 1\n");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );

        let second = apply(&state, false).await;
        assert_eq!(
            second["changed"], 0,
            "a second apply changes nothing: {second}"
        );
        assert_eq!(second["failed"], 0);
    }

    #[tokio::test]
    async fn a_changed_file_is_rewritten_and_absent_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x");
        std::fs::write(&p, "old").unwrap();
        let r = apply(
            &json!({ "resources": [{ "id": "x", "kind": "file", "path": p, "content": "new" }] }),
            false,
        )
        .await;
        assert_eq!(r["resources"][0]["status"], "changed");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new");
        let gone = apply(
            &json!({ "resources": [{ "id": "x", "kind": "file", "path": p, "state": "absent" }] }),
            false,
        )
        .await;
        assert_eq!(gone["resources"][0]["status"], "changed");
        assert!(!p.exists());
    }

    #[tokio::test]
    async fn an_unguarded_command_is_refused_and_failure_stops_the_run() {
        let r = apply(
            &json!({ "resources": [
                { "id": "bad", "kind": "command", "run": ["true"] },
                { "id": "after", "kind": "directory", "path": "/nonexistent-never-reached" },
            ]}),
            false,
        )
        .await;
        assert_eq!(r["failed"], 1);
        assert_eq!(
            r["resources"].as_array().unwrap().len(),
            1,
            "the run stops at the first failure"
        );
    }

    #[tokio::test]
    async fn duplicate_ids_and_unknown_kinds_fail() {
        let r = apply(
            &json!({ "continue_on_failure": true, "resources": [
                { "id": "a", "kind": "mystery" },
                { "id": "a", "kind": "directory", "path": "/tmp" },
            ]}),
            true,
        )
        .await;
        assert_eq!(r["failed"], 2, "{r}");
    }
}
