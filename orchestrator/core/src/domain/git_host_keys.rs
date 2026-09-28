// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SSH host keys of a git repository's remote (ADR-081)
//!
//! A clone or push over SSH checks the host's key before anything else, so a
//! clone cannot be answered by the wrong host and a deploy key is never
//! offered to one. The keys a remote is checked against are:
//!
//! - the keys given to the binding when it was created, when there are any;
//! - otherwise, for GitHub, GitLab and Bitbucket, the keys those hosts
//!   publish, pinned below;
//! - otherwise none, and the clone is refused before it starts.
//!
//! There is no trust on first use and no setting that turns the check off.

use base64::Engine;
use serde::{Deserialize, Serialize};

/// The published SSH host keys of the well-known git hosts.
///
/// Read on 2026-09-28 at 20:09 UTC from each host's own publication, and
/// checked against the fingerprints each publishes and against the keys the
/// hosts presented from this network:
///
/// - `github.com`: <https://api.github.com/meta> (`ssh_keys`; fingerprints in
///   `ssh_key_fingerprints` and at
///   <https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/githubs-ssh-key-fingerprints>)
/// - `gitlab.com`: <https://docs.gitlab.com/user/gitlab_com/> (SSH
///   `known_hosts` entries and fingerprints)
/// - `bitbucket.org`: <https://bitbucket.org/site/ssh>
///
/// When a host rotates a key, this list changes in a release.
const WELL_KNOWN_HOST_KEYS: &[(&str, &[&str])] = &[
    (
        "github.com",
        &[
            // SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl",
            // SHA256:p2QAMXNIC1TJYWeIOttrVc98/R1BUFWu3/LiyKgUfQM
            "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBEmKSENjQEezOmxkZMy7opKgwFB9nkt5YRrYMjNuG5N87uRgg6CLrbo5wAdT/y6v0mKV0U2w0WZ2YB/++Tpockg=",
            // SHA256:uNiVztksCsDhcc0u9e8BujQXVUpKZIDTMczCvj3tD2s
            "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQCj7ndNxQowgcQnjshcLrqPEiiphnt+VTTvDP6mHBL9j1aNUkY4Ue1gvwnGLVlOhGeYrnZaMgRK6+PKCUXaDbC7qtbW8gIkhL7aGCsOr/C56SJMy/BCZfxd1nWzAOxSDPgVsmerOBYfNqltV9/hWCqBywINIR+5dIg6JTJ72pcEpEjcYgXkE2YEFXV1JHnsKgbLWNlhScqb2UmyRkQyytRLtL+38TGxkxCflmO+5Z8CSSNY7GidjMIZ7Q4zMjA2n1nGrlTDkzwDCsw+wqFPGQA179cnfGWOWRVruj16z6XyvxvjJwbz0wQZ75XK5tKSb7FNyeIEs4TT4jk+S4dhPeAUC5y+bDYirYgM4GC7uEnztnZyaVWQ7B381AK4Qdrwt51ZqExKbQpTUNn+EjqoTwvqNj4kqx5QUCI0ThS/YkOxJCXmPUWZbhjpCg56i+2aB6CmK2JGhn57K5mj0MNdBXA4/WnwH6XoPWJzK5Nyu2zB3nAZp+S5hpQs+p1vN1/wsjk=",
        ],
    ),
    (
        "gitlab.com",
        &[
            // SHA256:eUXGGm1YGsMAS7vkcx6JOJdOGHPem5gQp4taiCfCLB8
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAfuCHKVTjquxvt6CM6tdG4SLp1Btn/nOeHHE5UOzRdf",
            // SHA256:HbW3g8zUjNSksFbqTiUWPWg2Bq1x8xdGUrliXFzSnUw
            "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBFSMqzJeV9rUzU4kWitGjeR4PWSa29SPqJ1fVkhtj3Hw9xjLVXVYrU9QlYWrOLXBpQ6KWjbjTDTdDkoohFzgbEY=",
            // SHA256:ROQFvPThGrW4RuWLoL9tq9I9zJ42fK4XywyRtbOz/EQ
            "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQCsj2bNKTBSpIYDEGk9KxsGh3mySTRgMtXL583qmBpzeQ+jqCMRgBqB98u3z++J1sKlXHWfM9dyhSevkMwSbhoR8XIq/U0tCNyokEi/ueaBMCvbcTHhO7FcwzY92WK4Yt0aGROY5qX2UKSeOvuP4D6TPqKF1onrSzH9bx9XUf2lEdWT/ia1NEKjunUqu1xOB/StKDHMoX4/OKyIzuS0q/T1zOATthvasJFoPrAjkohTyaDUz2LN5JoH839hViyEG82yB+MjcFV5MU3N1l1QL3cVUCh93xSaua1N85qivl+siMkPGbO5xR/En4iEY6K2XPASUEMaieWVNTRCtJ4S8H+9",
        ],
    ),
    (
        "bitbucket.org",
        &[
            // SHA256:ybgmFkzwOSotHTHLJgHO0QN8L0xErw6vd0VhFA9m3SM
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIazEu89wgQZ4bqs3d63QSMzYVa0MuJ2e2gKTKqu+UUO",
            // SHA256:FC73VB6C4OQLSCrjEayhMp9UMxS97caD/Yyi2bhW/J0
            "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBPIQmuzMBuKdWeF4+a2sjSSpBK0iqitSQ+5BM9KhpexuGt20JpTVM7u5BDZngncgrqDMbWdxMWWOGtZ9UgbqgZE=",
            // SHA256:46OSHA1Rmj8E8ERTC6xkNcmGOw9oFxYr0WF6zWW8l1E
            "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQDQeJzhupRu0u0cdegZIa8e86EG2qOCsIsD1Xw0xSeiPDlCr7kq97NLmMbpKTX6Esc30NuoqEEHCuc7yWtwp8dI76EEEB1VqY9QJq6vk+aySyboD5QF61I/1WeTwu+deCbgKMGbUijeXhtfbxSxm6JwGrXrhBdofTsbKRUsrN1WoNgUa8uqN1Vx6WAJw1JHPhglEGGHea6QICwJOAr/6mrui/oB7pkaWKHj3z7d1IC4KWLtY47elvjbaTlkN04Kc/5LFEirorGYVbt15kAUlqGM65pk6ZBxtaO3+30LVlORZkxOh+LKL/BvbZ/iRNhItLqNyieoQj/uh/7Iv4uyH/cV/0b4WDSd3DptigWq84lJubb9t/DnZlrJazxyDCulTmKdOR7vs9gMTo+uoIrPSb8ScTtvw65+odKAlBj59dhnVp9zd7QUojOpXlL62Aw56U4oO+FALuevvMjiWeavKhJqlR7i5n9srYcrNV7ttmDw7kf/97P5zauIhxcjX+xHv4M=",
        ],
    ),
];

/// The key types a host key may have.
const KEY_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "ssh-rsa",
];

/// One SSH host public key, as OpenSSH writes it: `<type> <base64>`.
///
/// Made only by [`SshHostKey::parse`], which checks that the base64 decodes
/// and that the key it holds is of the type named. Serialised as that line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SshHostKey {
    key_type: String,
    blob: Vec<u8>,
}

impl SshHostKey {
    /// Read a host key from a public key line (`ssh-ed25519 AAAA… [comment]`)
    /// or a `known_hosts` line (`host ssh-ed25519 AAAA…`). The host name of a
    /// `known_hosts` line and any comment are not kept.
    pub fn parse(line: &str) -> Result<Self, String> {
        let words: Vec<&str> = line.split_whitespace().collect();
        let start = words
            .iter()
            .position(|w| KEY_TYPES.contains(w))
            .ok_or_else(|| {
                format!(
                    "an SSH host key must be a line such as `ssh-ed25519 AAAA…`; its type must be one of {}",
                    KEY_TYPES.join(", ")
                )
            })?;
        if start > 1 {
            return Err("an SSH host key line has more than a host name before its type".into());
        }
        let key_type = words[start];
        let encoded = words
            .get(start + 1)
            .ok_or_else(|| format!("the {key_type} host key has no key after its type"))?;
        let blob = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| format!("the {key_type} host key is not valid base64"))?;
        // The key's own encoding starts with its type, as a length-prefixed
        // string: the line's type must be the key's.
        let named = blob
            .get(..4)
            .map(|len| u32::from_be_bytes([len[0], len[1], len[2], len[3]]) as usize)
            .and_then(|len| blob.get(4..4 + len));
        if named != Some(key_type.as_bytes()) {
            return Err(format!("the host key is not a {key_type} key"));
        }
        Ok(Self {
            key_type: key_type.to_string(),
            blob,
        })
    }

    /// The key type, such as `ssh-ed25519`.
    pub fn key_type(&self) -> &str {
        &self.key_type
    }

    /// The key as the SSH protocol carries it, which is what a server
    /// presents during the key exchange.
    pub fn blob(&self) -> &[u8] {
        &self.blob
    }

    /// The key as a public key line, `<type> <base64>`.
    pub fn to_line(&self) -> String {
        format!(
            "{} {}",
            self.key_type,
            base64::engine::general_purpose::STANDARD.encode(&self.blob)
        )
    }
}

impl TryFrom<String> for SshHostKey {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<SshHostKey> for String {
    fn from(value: SshHostKey) -> Self {
        value.to_line()
    }
}

/// The host and port of a repository URL that is reached over SSH:
/// `user@host:path` (port 22) or `ssh://[user@]host[:port]/path`. `None` for
/// any other URL.
pub fn ssh_remote(repo_url: &str) -> Option<(String, u16)> {
    let url = repo_url.trim();
    if url.starts_with("ssh://") {
        let parsed = url::Url::parse(url).ok()?;
        let host = parsed.host_str()?.trim_matches(['[', ']']).to_string();
        return Some((host, parsed.port().unwrap_or(22)));
    }
    if url.contains("://") {
        return None;
    }
    // scp-like: `[user@]host:path`, where the `:` comes before any `/`.
    let colon = url.find(':')?;
    if url.find('/').is_some_and(|slash| slash < colon) {
        return None;
    }
    let authority = &url[..colon];
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    (!host.is_empty()).then(|| (host.to_string(), 22))
}

/// The published keys of `host` when it is one of the well-known git hosts.
pub fn well_known_host_keys(host: &str) -> Option<Vec<SshHostKey>> {
    let host = host.to_ascii_lowercase();
    WELL_KNOWN_HOST_KEYS
        .iter()
        .find(|(name, _)| *name == host)
        .map(|(_, lines)| {
            lines
                .iter()
                .map(|line| SshHostKey::parse(line).expect("a pinned host key is valid"))
                .collect()
        })
}

/// The keys the SSH remote of `repo_url` is checked against, given the keys
/// its binding holds. `Ok(None)` when the URL is not reached over SSH.
///
/// The error is the sentence a person reads when a binding has no key for a
/// host that is not well known.
pub fn host_keys_for(
    repo_url: &str,
    binding_keys: &[SshHostKey],
) -> Result<Option<Vec<SshHostKey>>, String> {
    let Some((host, _)) = ssh_remote(repo_url) else {
        return Ok(None);
    };
    if !binding_keys.is_empty() {
        return Ok(Some(binding_keys.to_vec()));
    }
    well_known_host_keys(&host).map(Some).ok_or_else(|| {
        format!(
            "the SSH host {host} has no known host key. Its key is checked before anything is \
             sent to it, so the repository needs the host's public key in `ssh_host_keys`: a line \
             such as `ssh-ed25519 AAAA…`, as the host publishes it. A repository added without \
             one must be removed and added again with it"
        )
    })
}

/// The `known_hosts` file for `keys` of the SSH remote of `repo_url`, as
/// OpenSSH reads it: one line per key, the host written `[host]:port` when
/// the port is not 22.
pub fn known_hosts_file(repo_url: &str, keys: &[SshHostKey]) -> Option<String> {
    let (host, port) = ssh_remote(repo_url)?;
    let name = if port == 22 {
        host
    } else {
        format!("[{host}]:{port}")
    };
    Some(
        keys.iter()
            .map(|key| format!("{name} {}\n", key.to_line()))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pinned_key_is_valid_and_of_the_type_named() {
        for (host, lines) in WELL_KNOWN_HOST_KEYS {
            assert_eq!(lines.len(), 3, "{host} has three pinned keys");
            for line in *lines {
                let key = SshHostKey::parse(line).unwrap_or_else(|e| panic!("{host}: {e}"));
                assert_eq!(&key.to_line(), line, "{host}: the key reads back as pinned");
            }
        }
    }

    #[test]
    fn ssh_remotes_are_recognised_and_other_urls_are_not() {
        let cases = [
            ("git@github.com:o/r.git", Some(("github.com", 22))),
            ("ssh://git@gitlab.com/o/r.git", Some(("gitlab.com", 22))),
            (
                "ssh://git@git.example.invalid:2222/o/r.git",
                Some(("git.example.invalid", 2222)),
            ),
            ("ssh://127.0.0.1:2200/srv/r.git", Some(("127.0.0.1", 2200))),
            ("https://github.com/o/r.git", None),
            ("http://github.com/o/r.git", None),
            ("/srv/repos/r.git", None),
            ("./r.git", None),
        ];
        for (url, expected) in cases {
            assert_eq!(
                ssh_remote(url),
                expected.map(|(h, p)| (h.to_string(), p)),
                "{url}"
            );
        }
    }

    #[test]
    fn well_known_hosts_need_no_key_and_other_hosts_do() {
        for url in [
            "git@github.com:o/r.git",
            "git@GitLab.com:o/r.git",
            "git@bitbucket.org:o/r.git",
        ] {
            let keys = host_keys_for(url, &[]).unwrap().unwrap();
            assert_eq!(keys.len(), 3, "{url}");
        }
        let error = host_keys_for("git@git.example.invalid:o/r.git", &[]).unwrap_err();
        assert!(
            error.contains("git.example.invalid") && error.contains("ssh_host_keys"),
            "the refusal names the host and what to add: {error}"
        );
        assert_eq!(
            host_keys_for("https://git.example.invalid/o/r.git", &[]),
            Ok(None)
        );
    }

    #[test]
    fn keys_given_to_the_binding_are_the_ones_checked() {
        let given = SshHostKey::parse(
            "git.example.invalid ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAfuCHKVTjquxvt6CM6tdG4SLp1Btn/nOeHHE5UOzRdf",
        )
        .unwrap();
        let keys = host_keys_for("git@github.com:o/r.git", std::slice::from_ref(&given))
            .unwrap()
            .unwrap();
        assert_eq!(keys, vec![given.clone()]);
        assert_eq!(
            known_hosts_file("ssh://git@git.example.invalid:2222/o/r.git", &keys).unwrap(),
            format!("[git.example.invalid]:2222 {}\n", given.to_line())
        );
    }

    #[test]
    fn a_line_that_is_not_a_host_key_is_refused() {
        for line in [
            "",
            "AAAAC3NzaC1lZDI1NTE5AAAAIAfuCHKVTjquxvt6CM6tdG4SLp1Btn/nOeHHE5UOzRdf",
            "ssh-ed25519",
            "ssh-ed25519 not*base64",
            // An RSA key's text under the ed25519 type.
            "ssh-ed25519 AAAAB3NzaC1yc2EAAAADAQABAAABAQCsj2bNKTBSpIYDEGk9KxsGh3mySTRgMtXL583qmBpzeQ",
            "a b ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAfuCHKVTjquxvt6CM6tdG4SLp1Btn/nOeHHE5UOzRdf",
        ] {
            assert!(SshHostKey::parse(line).is_err(), "{line:?} was accepted");
        }
    }
}
