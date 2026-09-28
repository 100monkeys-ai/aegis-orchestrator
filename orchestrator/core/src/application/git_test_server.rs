// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! A git server for tests that demands a password.
//!
//! It serves one bare repository over HTTP on the loopback interface, passes
//! each request to `git http-backend` as a CGI program, and refuses every
//! request whose `Authorization` header does not carry the user name and
//! password it was started with. It is stopped, and its files removed, when
//! the value is dropped.
//!
//! Tests use it to run a real clone and fetch with a credential and then look
//! for that credential everywhere it must not be.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use base64::Engine;

/// A running test git server. Dropping it stops the server and removes the
/// repositories.
pub(crate) struct GitTestServer {
    addr: SocketAddr,
    root: tempfile::TempDir,
    stop: Arc<AtomicBool>,
    authorized: Arc<AtomicUsize>,
    refused: Arc<AtomicUsize>,
    handle: Option<JoinHandle<()>>,
}

impl GitTestServer {
    /// Start a server holding `repo.git` with one commit on `main`, which
    /// accepts only `username` and `password`.
    pub(crate) fn start(username: &str, password: &str) -> Self {
        let root = tempfile::tempdir().expect("temp dir for the git server");
        let bare = root.path().join("repo.git");
        let work = root.path().join("work");
        run_git(
            root.path(),
            &["init", "--bare", "--initial-branch=main", "repo.git"],
        );
        run_git(root.path(), &["init", "--initial-branch=main", "work"]);
        std::fs::write(work.join("README.md"), "first\n").unwrap();
        run_git(&work, &["add", "README.md"]);
        run_git(&work, &["commit", "-m", "first"]);
        run_git(&work, &["push", bare.to_str().unwrap(), "main"]);

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let authorized = Arc::new(AtomicUsize::new(0));
        let refused = Arc::new(AtomicUsize::new(0));
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
        );
        let project_root = root.path().to_path_buf();
        let backend = http_backend_path();
        let handle = {
            let stop = stop.clone();
            let authorized = authorized.clone();
            let refused = refused.clone();
            let remote_user = username.to_string();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let expected = expected.clone();
                    let project_root = project_root.clone();
                    let backend = backend.clone();
                    let authorized = authorized.clone();
                    let refused = refused.clone();
                    let remote_user = remote_user.clone();
                    std::thread::spawn(move || {
                        let _ = serve_one(
                            stream,
                            &expected,
                            &project_root,
                            &backend,
                            &remote_user,
                            &authorized,
                            &refused,
                        );
                    });
                }
            })
        };

        Self {
            addr,
            root,
            stop,
            authorized,
            refused,
            handle: Some(handle),
        }
    }

    /// The repository's URL, with no user info.
    pub(crate) fn url(&self) -> String {
        format!("http://{}/repo.git", self.addr)
    }

    /// Requests the server answered because they carried the right password.
    pub(crate) fn authorized_requests(&self) -> usize {
        self.authorized.load(Ordering::SeqCst)
    }

    /// Requests the server refused for a missing or wrong password.
    pub(crate) fn refused_requests(&self) -> usize {
        self.refused.load(Ordering::SeqCst)
    }

    /// Add a commit on `main` of the served repository and return its id.
    pub(crate) fn add_commit(&self, file: &str, content: &str) -> String {
        let work = self.root.path().join("work");
        std::fs::write(work.join(file), content).unwrap();
        run_git(&work, &["add", file]);
        run_git(&work, &["commit", "-m", file]);
        let bare = self.root.path().join("repo.git");
        run_git(&work, &["push", bare.to_str().unwrap(), "main"]);
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&work)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// The id of `main` in the served repository.
    pub(crate) fn head(&self) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "main"])
            .current_dir(self.root.path().join("repo.git"))
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }
}

impl Drop for GitTestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the flag.
        let _ = TcpStream::connect(self.addr);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed in the test fixture");
}

fn http_backend_path() -> PathBuf {
    let out = Command::new("git")
        .arg("--exec-path")
        .output()
        .expect("git --exec-path");
    PathBuf::from(String::from_utf8(out.stdout).unwrap().trim()).join("git-http-backend")
}

fn serve_one(
    stream: TcpStream,
    expected_auth: &str,
    project_root: &Path,
    backend: &Path,
    remote_user: &str,
    authorized: &AtomicUsize,
    refused: &AtomicUsize,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let mut headers: HashMap<String, String> = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let mut out = stream;
    if headers.get("authorization").map(String::as_str) != Some(expected_auth) {
        refused.fetch_add(1, Ordering::SeqCst);
        out.write_all(
            b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"test\"\r\n\
              Content-Length: 0\r\nConnection: close\r\n\r\n",
        )?;
        let _ = out.shutdown(Shutdown::Both);
        return Ok(());
    }
    authorized.fetch_add(1, Ordering::SeqCst);

    let body = if headers
        .get("transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
    {
        read_chunked(&mut reader)?
    } else {
        let len: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut b = vec![0u8; len];
        reader.read_exact(&mut b)?;
        b
    };

    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let mut cmd = Command::new(backend);
    cmd.env_clear()
        .env("GIT_PROJECT_ROOT", project_root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("REQUEST_METHOD", &method)
        .env("PATH_INFO", path)
        .env("QUERY_STRING", query)
        .env("REMOTE_USER", remote_user)
        .env("REMOTE_ADDR", "127.0.0.1")
        .env("CONTENT_LENGTH", body.len().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Ok(p) = std::env::var("PATH") {
        cmd.env("PATH", p);
    }
    if let Some(v) = headers.get("content-type") {
        cmd.env("CONTENT_TYPE", v);
    }
    if let Some(v) = headers.get("content-encoding") {
        cmd.env("HTTP_CONTENT_ENCODING", v);
    }
    if let Some(v) = headers.get("git-protocol") {
        cmd.env("HTTP_GIT_PROTOCOL", v);
    }
    let mut child = cmd.spawn()?;
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(&body)?;
    }
    let output = child.wait_with_output()?;
    let raw = output.stdout;

    let (head_end, sep_len) = find_header_end(&raw).unwrap_or((raw.len(), 0));
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let payload = &raw[(head_end + sep_len).min(raw.len())..];
    let mut status = "200 OK".to_string();
    let mut response = String::new();
    for line in head.lines() {
        if let Some(s) = line.strip_prefix("Status:") {
            status = s.trim().to_string();
        } else if !line.is_empty() {
            response.push_str(line);
            response.push_str("\r\n");
        }
    }
    out.write_all(
        format!(
            "HTTP/1.1 {status}\r\n{response}Content-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        )
        .as_bytes(),
    )?;
    out.write_all(payload)?;
    out.flush()?;
    let _ = out.shutdown(Shutdown::Both);
    Ok(())
}

fn find_header_end(raw: &[u8]) -> Option<(usize, usize)> {
    if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
        return Some((i, 4));
    }
    raw.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2))
}

fn read_chunked<R: BufRead>(reader: &mut R) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        reader.read_line(&mut size_line)?;
        let size_hex = size_line.trim().split(';').next().unwrap_or("0");
        let size = usize::from_str_radix(size_hex, 16).unwrap_or(0);
        if size == 0 {
            // Trailer section ends with an empty line.
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l)? == 0 || l.trim().is_empty() {
                    break;
                }
            }
            return Ok(body);
        }
        let mut chunk = vec![0u8; size];
        reader.read_exact(&mut chunk)?;
        body.extend_from_slice(&chunk);
        let mut crlf = String::new();
        reader.read_line(&mut crlf)?;
    }
}

/// Every place a test looks for a credential: `true` when `haystack` holds
/// `secret` or any run of 8 of its characters.
pub(crate) fn holds_any_part_of(haystack: &str, secret: &str) -> bool {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() < 8 {
        return !secret.is_empty() && haystack.contains(secret);
    }
    chars.windows(8).any(|w| {
        let run: String = w.iter().collect();
        haystack.contains(&run)
    })
}

/// Read every file under `dir` (the `.git` directory included) and return the
/// paths of those that hold `secret` or any 8-character run of it.
pub(crate) fn files_holding(dir: &Path, secret: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() {
                if let Ok(bytes) = std::fs::read(&p) {
                    if holds_any_part_of(&String::from_utf8_lossy(&bytes), secret) {
                        found.push(p);
                    }
                }
            }
        }
    }
    found
}

// ── Step runners for tests ───────────────────────────────────────────────────

use crate::domain::runtime::{
    ContainerStepConfig, ContainerStepError, ContainerStepResult, ContainerStepRunner,
};
use std::sync::Mutex;

/// A [`ContainerStepRunner`] that runs the step's entrypoint and command with
/// the host's shell instead of in a container. It applies the same
/// environment filter the container runner applies, gives the step a home
/// directory of its own (so no git configuration of the machine running the
/// tests takes part), and hands the step its standard input the way the
/// container runner does. It keeps every configuration it was given.
pub(crate) struct HostShellRunner {
    home: tempfile::TempDir,
    extra_env: Vec<(String, String)>,
    pub(crate) seen: Mutex<Vec<ContainerStepConfig>>,
}

impl HostShellRunner {
    pub(crate) fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("home for the step"),
            extra_env: Vec::new(),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// Add a variable the step's processes see. `PATH` is prepended to the
    /// host's.
    pub(crate) fn with_env(mut self, key: &str, value: &str) -> Self {
        self.extra_env.push((key.to_string(), value.to_string()));
        self
    }

    /// The `Debug` rendering of every configuration the runner was given.
    pub(crate) fn seen_debug(&self) -> String {
        format!("{:?}", self.seen.lock().unwrap())
    }
}

#[async_trait::async_trait]
impl ContainerStepRunner for HostShellRunner {
    async fn run_step(
        &self,
        config: ContainerStepConfig,
    ) -> Result<ContainerStepResult, ContainerStepError> {
        self.seen.lock().unwrap().push(config.clone());
        let mut argv: Vec<String> = config.entrypoint.clone().unwrap_or_default();
        argv.extend(config.command.iter().cloned());
        let (env, _blocked) = crate::domain::env_guard::filter_env_vars(&config.env);
        let home = self.home.path().to_path_buf();
        let extra_env = self.extra_env.clone();
        let stdin = config.stdin.as_ref().map(|b| b.expose().to_vec());
        tokio::task::spawn_blocking(move || {
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..]).env_clear();
            let mut path = std::env::var("PATH").unwrap_or_default();
            for (k, v) in &extra_env {
                if k == "PATH" {
                    path = format!("{v}:{path}");
                }
            }
            cmd.env("PATH", path)
                .env("HOME", &home)
                .env("GIT_CONFIG_NOSYSTEM", "1");
            for (k, v) in &extra_env {
                if k != "PATH" {
                    cmd.env(k, v);
                }
            }
            cmd.envs(env.iter());
            cmd.stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
            let started = std::time::Instant::now();
            let mut child = cmd
                .spawn()
                .map_err(|e| ContainerStepError::DockerError(format!("spawn: {e}")))?;
            if let Some(bytes) = stdin {
                let mut input = child.stdin.take().unwrap();
                input
                    .write_all(&bytes)
                    .map_err(|e| ContainerStepError::DockerError(format!("stdin: {e}")))?;
            }
            let out = child
                .wait_with_output()
                .map_err(|e| ContainerStepError::DockerError(format!("wait: {e}")))?;
            Ok(ContainerStepResult {
                exit_code: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).to_string(),
                duration_ms: started.elapsed().as_millis() as u64,
            })
        })
        .await
        .map_err(|e| ContainerStepError::DockerError(format!("join: {e}")))?
    }
}

/// Watches the host's process table while a test runs: every process's
/// argument list and environment the test's user can read, read over and
/// over until stopped. Reports where a credential was seen, never the
/// credential.
pub(crate) struct ProcessWatch {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<(Vec<String>, usize)>>,
}

impl ProcessWatch {
    pub(crate) fn start(secret: &str) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let secret = secret.to_string();
        let handle = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut hits: Vec<String> = Vec::new();
                let mut rounds = 0usize;
                loop {
                    rounds += 1;
                    if let Ok(entries) = std::fs::read_dir("/proc") {
                        for e in entries.flatten() {
                            let name = e.file_name().to_string_lossy().to_string();
                            if !name.chars().all(|c| c.is_ascii_digit()) {
                                continue;
                            }
                            for file in ["cmdline", "environ"] {
                                if let Ok(bytes) = std::fs::read(e.path().join(file)) {
                                    let text = String::from_utf8_lossy(&bytes);
                                    if holds_any_part_of(&text, &secret) {
                                        let hit = format!("/proc/{name}/{file}");
                                        if !hits.contains(&hit) {
                                            hits.push(hit);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if stop.load(Ordering::SeqCst) {
                        return (hits, rounds);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            })
        };
        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// Stop watching; return where the credential was seen and how many
    /// times the table was read.
    pub(crate) fn stop(mut self) -> (Vec<String>, usize) {
        self.stop.store(true, Ordering::SeqCst);
        self.handle.take().unwrap().join().unwrap()
    }
}

impl Drop for ProcessWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// A container engine for a test that needs one, or `None` when there is
/// none and none is required. Where `AEGIS_TEST_DOCKER` is set (CI sets it)
/// a missing engine fails the test instead, so these tests cannot pass there
/// without running.
pub(crate) async fn container_engine(test: &str) -> Option<bollard::Docker> {
    let required = std::env::var_os("AEGIS_TEST_DOCKER").is_some();
    let reached = match bollard::Docker::connect_with_local_defaults() {
        Ok(docker) => match docker.ping().await {
            Ok(_) => Ok(docker),
            Err(e) => Err(e.to_string()),
        },
        Err(e) => Err(e.to_string()),
    };
    match reached {
        Ok(docker) => Some(docker),
        Err(e) if required => {
            panic!("{test}: AEGIS_TEST_DOCKER is set and no container engine answered: {e}")
        }
        Err(e) => {
            eprintln!(
                "SKIPPED {test}: no container engine answered ({e}). This test ran nothing. \
                 Set AEGIS_TEST_DOCKER=1 where an engine is present; CI does."
            );
            None
        }
    }
}

/// The real container step runner, on `docker`, with no volume transport.
pub(crate) fn container_runner(
    docker: bollard::Docker,
    event_bus: Arc<crate::infrastructure::event_bus::EventBus>,
) -> Arc<crate::infrastructure::container_step_runner::ContainerStepRunnerImpl> {
    use crate::infrastructure::container_step_runner::{
        ContainerStepRunnerConfig, ContainerStepRunnerImpl,
    };
    use crate::infrastructure::image_manager::{
        NodeConfigCredentialResolver, StandardDockerImageManager,
    };
    use crate::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};
    let image_manager = Arc::new(StandardDockerImageManager::new(
        docker.clone(),
        Arc::new(NodeConfigCredentialResolver::new(vec![])),
    ));
    let secrets = Arc::new(SecretsManager::from_store(
        Arc::new(TestSecretStore::new()),
        event_bus.clone(),
    ));
    Arc::new(ContainerStepRunnerImpl::new(
        docker,
        image_manager,
        ContainerStepRunnerConfig {
            network_mode: None,
            fuse_daemon: None,
            fuse_mount_prefix: "/tmp/aegis-fuse-mounts".to_string(),
            fuse_mount_client: None,
        },
        event_bus,
        secrets,
        Arc::new(crate::application::nfs_gateway::NfsVolumeRegistry::new()),
        None,
    ))
}

/// A marker credential made for one test: 34 letters, none of them a hex
/// digit, so no object id or hash in a repository can hold a run of it.
pub(crate) fn marker(prefix: &str) -> String {
    let hex = uuid::Uuid::new_v4().simple().to_string();
    let body: String = hex
        .chars()
        .map(|c| (b'g' + c.to_digit(16).unwrap() as u8) as char)
        .collect();
    format!("{prefix}{body}")
}
