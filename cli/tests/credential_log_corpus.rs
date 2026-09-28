// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Corpus test: no output site renders a raw connection string.
//!
//! A connection URL can carry a credential in its user info
//! (`postgres://user:password@host/db`) or in its query string
//! (`?password=…`, `?api_key=…`). Rendered raw into a log line, a span field,
//! a panic or an error message, the credential goes wherever the daemon's
//! output goes and stays there for as long as that output is retained.
//!
//! This test walks every non-test source file of the workspace and reads every
//! argument of every `tracing` event and span macro, every `#[instrument]`
//! attribute, and — in daemon code — every `println!`, `eprintln!`, `print!`,
//! `eprint!`, `panic!`, `anyhow!` and `bail!`. An argument is a connection
//! string when its field name, or any lower-case identifier in its
//! expression, or any inline `{capture}` in its format string, is named like
//! one: `url`, `uri`, `dsn`, `endpoint`, `connection`, `conn_str`,
//! `connection_string`, or anything ending `_url`, `_uri`, `_dsn`,
//! `_endpoint`, `_conn_str` or `_connection_string`.
//!
//! Such an argument must visibly pass through the redaction seam in
//! `aegis_orchestrator_core::domain::secrets`: its expression contains
//! `SensitiveUrl`, `.redacted()` (a method only `SensitiveUrl` has),
//! `redact_url(` or `sanitize_url(`. Anything else is reported, every
//! offending site in one run.

use std::path::{Path, PathBuf};

/// Crate source roots walked, relative to the workspace root.
const ROOTS: &[&str] = &[
    "cli/src",
    "orchestrator/core/src",
    "orchestrator/swarm/src",
    "sdks/src",
];

/// `tracing` macros whose arguments reach a subscriber.
const TRACING_MACROS: &[&str] = &[
    "trace",
    "debug",
    "info",
    "warn",
    "error",
    "event",
    "span",
    "trace_span",
    "debug_span",
    "info_span",
    "warn_span",
    "error_span",
];

/// Macros whose output reaches the process's standard streams or an error
/// that is logged. Checked in daemon code only: the interactive CLI prints
/// URLs its user typed, or must open, to that user's own terminal.
const PROCESS_OUTPUT_MACROS: &[&str] = &[
    "println", "eprintln", "print", "eprint", "panic", "anyhow", "bail",
];

/// Text whose presence in an argument's expression shows it passes through
/// the redaction seam.
const REDACTION_MARKERS: &[&str] = &[
    "SensitiveUrl",
    ".redacted()",
    "redact_url(",
    "sanitize_url(",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the cli crate sits one level below the workspace root")
        .to_path_buf()
}

/// Is `ident` named like a connection string?
fn is_connection_name(ident: &str) -> bool {
    if !ident.starts_with(|c: char| c.is_ascii_lowercase() || c == '_') {
        return false;
    }
    const EXACT: &[&str] = &[
        "url",
        "uri",
        "dsn",
        "endpoint",
        "connection",
        "conn_str",
        "connection_string",
    ];
    const SUFFIXES: &[&str] = &[
        "_url",
        "_uri",
        "_dsn",
        "_endpoint",
        "_conn_str",
        "_connection_string",
    ];
    EXACT.contains(&ident) || SUFFIXES.iter().any(|s| ident.ends_with(s))
}

/// Is this file daemon code, where process-output macros are checked too?
fn is_daemon_code(relative: &str) -> bool {
    relative.starts_with("orchestrator/") || relative.contains("daemon")
}

// ── Lexing ──────────────────────────────────────────────────────────────────

/// Replace every comment with spaces (newlines kept, so line numbers hold),
/// leaving string and character literals intact.
fn blank_comments(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    out[i] = b' ';
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut depth = 0usize;
                while i < b.len() {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        out[i] = b' ';
                        out[i + 1] = b' ';
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        out[i] = b' ';
                        out[i + 1] = b' ';
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        if b[i] != b'\n' {
                            out[i] = b' ';
                        }
                        i += 1;
                    }
                }
            }
            b'"' | b'\'' | b'r' => {
                i = skip_literal(b, i).unwrap_or(i + 1);
            }
            _ => i += 1,
        }
    }
    String::from_utf8(out).expect("only ASCII bytes were replaced")
}

/// If a string, raw string or character literal starts at `i`, return the
/// index just past it.
fn skip_literal(b: &[u8], i: usize) -> Option<usize> {
    let prev_is_ident = i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
    match b[i] {
        b'"' => {
            let mut j = i + 1;
            while j < b.len() {
                match b[j] {
                    b'\\' => j += 2,
                    b'"' => return Some(j + 1),
                    _ => j += 1,
                }
            }
            Some(b.len())
        }
        b'r' if !prev_is_ident => {
            let mut j = i + 1;
            let mut hashes = 0;
            while b.get(j) == Some(&b'#') {
                hashes += 1;
                j += 1;
            }
            if b.get(j) != Some(&b'"') {
                return None;
            }
            j += 1;
            while j < b.len() {
                if b[j] == b'"'
                    && b[j + 1..]
                        .iter()
                        .take(hashes)
                        .filter(|c| **c == b'#')
                        .count()
                        == hashes
                {
                    return Some(j + 1 + hashes);
                }
                j += 1;
            }
            Some(b.len())
        }
        b'\'' => {
            // A character literal is 'x' or '\…'; anything else is a lifetime.
            if b.get(i + 1) == Some(&b'\\') {
                // Skip the backslash and the escaped character, then find the
                // closing quote (`'\''`, `'\n'`, `'\u{1F600}'`).
                let mut j = i + 3;
                while j < b.len() && b[j] != b'\'' {
                    j += 1;
                }
                return Some(j + 1);
            }
            let first = *b.get(i + 1)?;
            let len = match first {
                0x00..=0x7F => 1,
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                _ => 4,
            };
            if b.get(i + 1 + len) == Some(&b'\'') {
                Some(i + 2 + len)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Index of the delimiter closing the one at `open`, skipping literals.
fn matching_close(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < b.len() {
        if let Some(next) = skip_literal(b, i) {
            i = next;
            continue;
        }
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Replace every `#[cfg(test)] mod name { … }` with spaces, newlines kept.
fn blank_test_modules(src: &str) -> String {
    let mut out = src.as_bytes().to_vec();
    let mut from = 0;
    while let Some(pos) = src[from..].find("#[cfg(test)]") {
        let at = from + pos;
        let rest = src[at + "#[cfg(test)]".len()..].trim_start();
        let rest = rest.strip_prefix("pub(crate) ").unwrap_or(rest);
        let rest = rest.strip_prefix("pub ").unwrap_or(rest);
        if let Some(after_mod) = rest.strip_prefix("mod ") {
            let name_len = after_mod
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(after_mod.len());
            if after_mod[name_len..].trim_start().starts_with('{') {
                let brace = src[at..].find('{').map(|p| at + p);
                if let Some(close) = brace.and_then(|o| matching_close(src.as_bytes(), o)) {
                    for byte in &mut out[at..=close] {
                        if *byte != b'\n' {
                            *byte = b' ';
                        }
                    }
                    from = close + 1;
                    continue;
                }
            }
        }
        from = at + 1;
    }
    String::from_utf8(out).expect("only ASCII bytes were replaced")
}

/// Split at top-level commas, skipping literals and nested delimiters.
fn split_top_level(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        if let Some(next) = skip_literal(b, i) {
            i = next;
            continue;
        }
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                parts.push(s[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    let tail = s[start..].trim();
    if !tail.is_empty() {
        parts.push(tail.to_string());
    }
    parts
}

/// Byte ranges of the string literals in `s`, and the text outside them.
fn literals_and_code(s: &str) -> (Vec<String>, String) {
    let b = s.as_bytes();
    let mut literals = Vec::new();
    let mut code = String::new();
    let mut i = 0;
    while i < b.len() {
        if matches!(b[i], b'"' | b'r') {
            if let Some(next) = skip_literal(b, i) {
                literals.push(s[i..next].to_string());
                code.push(' ');
                i = next;
                continue;
            }
        }
        code.push(b[i] as char);
        i += 1;
    }
    (literals, code)
}

/// Inline `{capture}` identifiers of a format string literal.
fn inline_captures(literal: &str) -> Vec<String> {
    let mut names = Vec::new();
    let chars: Vec<char> = literal.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '{' {
            if chars.get(i + 1) == Some(&'{') {
                i += 2;
                continue;
            }
            let name: String = chars[i + 1..]
                .iter()
                .take_while(|c| c.is_ascii_alphanumeric() || **c == '_')
                .collect();
            if !name.is_empty() && !name.chars().all(|c| c.is_ascii_digit()) {
                names.push(name);
            }
        }
        i += 1;
    }
    names
}

fn identifiers(code: &str) -> Vec<&str> {
    code.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty() && !w.starts_with(|c: char| c.is_ascii_digit()))
        .collect()
}

/// The first `=` that is an assignment (not `==`, `=>`, `<=`, `>=`, `!=`)
/// outside literals and nested delimiters.
fn field_assignment(arg: &str) -> Option<usize> {
    let b = arg.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < b.len() {
        if let Some(next) = skip_literal(b, i) {
            i = next;
            continue;
        }
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'=' if depth == 0 => {
                let prev = if i > 0 { b[i - 1] } else { b' ' };
                let next = b.get(i + 1).copied().unwrap_or(b' ');
                if !matches!(prev, b'=' | b'<' | b'>' | b'!') && !matches!(next, b'=' | b'>') {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Why `arg` renders a raw connection string, or `None` if it does not.
fn raw_connection_string(arg: &str) -> Option<String> {
    let arg = arg.trim();
    // `target:`, `parent:`, `name:` and a level are metadata, not output.
    for meta in ["target:", "parent:", "name:"] {
        if arg.starts_with(meta) {
            return None;
        }
    }
    if REDACTION_MARKERS.iter().any(|m| arg.contains(m)) {
        return None;
    }
    let (field, expr) = match field_assignment(arg) {
        Some(eq) => (Some(arg[..eq].trim()), arg[eq + 1..].trim()),
        None => (None, arg),
    };
    let expr = expr.trim_start_matches(['%', '?']).trim();
    if let Some(field) = field {
        let last = field.rsplit('.').next().unwrap_or(field).trim();
        if is_connection_name(last) {
            return Some(format!("field `{last}`"));
        }
    }
    let (literals, code) = literals_and_code(expr);
    for literal in &literals {
        for capture in inline_captures(literal) {
            if is_connection_name(&capture) {
                return Some(format!("inline capture `{{{capture}}}`"));
            }
        }
    }
    identifiers(&code)
        .into_iter()
        .find(|ident| is_connection_name(ident))
        .map(|ident| format!("value `{ident}`"))
}

/// Line number (1-based) of byte offset `at`.
fn line_of(src: &str, at: usize) -> usize {
    src[..at].bytes().filter(|b| *b == b'\n').count() + 1
}

struct Scan {
    violations: Vec<String>,
    sites: usize,
}

/// Scan one file's (comment- and test-stripped) source.
fn scan_source(relative: &str, src: &str, scan: &mut Scan) {
    let b = src.as_bytes();
    let daemon = is_daemon_code(relative);
    let mut i = 0;
    while i < b.len() {
        if let Some(next) = skip_literal(b, i) {
            i = next;
            continue;
        }
        // `#[instrument(...)]` / `#[tracing::instrument(...)]`.
        if b[i] == b'#' && src[i..].starts_with("#[") {
            let head = &src[i + 2..];
            let head = head.strip_prefix("tracing::").unwrap_or(head);
            if head.starts_with("instrument") {
                if let Some(close) = matching_close(b, i + 1) {
                    scan_instrument(relative, src, i, close, scan);
                    i = close + 1;
                    continue;
                }
            }
        }
        if b[i].is_ascii_alphabetic()
            && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_'))
        {
            let end = i + src[i..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(src.len() - i);
            let name = &src[i..end];
            let rest = &src[end..];
            let checked =
                TRACING_MACROS.contains(&name) || (daemon && PROCESS_OUTPUT_MACROS.contains(&name));
            if checked && rest.starts_with('!') {
                let after_bang = rest[1..].trim_start();
                if after_bang.starts_with('(') {
                    let open = src.len() - after_bang.len();
                    if let Some(close) = matching_close(b, open) {
                        scan.sites += 1;
                        for arg in split_top_level(&src[open + 1..close]) {
                            if let Some(why) = raw_connection_string(&arg) {
                                scan.violations.push(format!(
                                    "{relative}:{}: {name}! renders {why} raw: {}",
                                    line_of(src, i),
                                    arg.split_whitespace().collect::<Vec<_>>().join(" ")
                                ));
                            }
                        }
                        i = close + 1;
                        continue;
                    }
                }
            }
            i = end;
            continue;
        }
        i += 1;
    }
}

/// Check an `#[instrument(...)]` attribute spanning `at..=close`: its
/// `fields(...)`, and every parameter it records because it is not skipped.
fn scan_instrument(relative: &str, src: &str, at: usize, close: usize, scan: &mut Scan) {
    scan.sites += 1;
    let attr = &src[at..=close];
    let line = line_of(src, at);
    let Some(open) = attr.find('(') else {
        // Bare `#[instrument]` records every parameter.
        check_recorded_params(relative, line, src, close, &[], scan);
        return;
    };
    let inner_close = matching_close(attr.as_bytes(), open).unwrap_or(attr.len() - 1);
    let mut skipped: Vec<String> = Vec::new();
    let mut skip_all = false;
    for part in split_top_level(&attr[open + 1..inner_close]) {
        if part == "skip_all" {
            skip_all = true;
        } else if let Some(list) = part.strip_prefix("skip(") {
            skipped.extend(
                list.strip_suffix(')')
                    .unwrap_or(list)
                    .split(',')
                    .map(|s| s.trim().to_string()),
            );
        } else if let Some(fields) = part.strip_prefix("fields(") {
            for field in split_top_level(fields.strip_suffix(')').unwrap_or(fields)) {
                if let Some(why) = raw_connection_string(&field) {
                    scan.violations.push(format!(
                        "{relative}:{line}: #[instrument] field renders {why} raw: {field}"
                    ));
                }
            }
        }
    }
    if !skip_all {
        check_recorded_params(relative, line, src, close, &skipped, scan);
    }
}

/// `#[instrument]` records every parameter it does not skip, by `Debug`.
fn check_recorded_params(
    relative: &str,
    line: usize,
    src: &str,
    attr_close: usize,
    skipped: &[String],
    scan: &mut Scan,
) {
    let after = &src[attr_close + 1..];
    let Some(fn_at) = after.find("fn ") else {
        return;
    };
    let Some(paren) = after[fn_at..].find('(').map(|p| attr_close + 1 + fn_at + p) else {
        return;
    };
    let Some(paren_close) = matching_close(src.as_bytes(), paren) else {
        return;
    };
    for param in split_top_level(&src[paren + 1..paren_close]) {
        let name = param
            .split(':')
            .next()
            .unwrap_or("")
            .trim()
            .trim_start_matches("mut ")
            .trim();
        if name.contains("self") || skipped.iter().any(|s| s == name) {
            continue;
        }
        if is_connection_name(name) {
            scan.violations.push(format!(
                "{relative}:{line}: #[instrument] records parameter `{name}` raw"
            ));
        }
    }
}

fn scan_file(root: &Path, path: &Path, scan: &mut Scan) {
    let relative = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if file_name.ends_with("_tests.rs") || file_name == "tests.rs" {
        return;
    }
    let src = std::fs::read_to_string(path).unwrap_or_default();
    let src = blank_test_modules(&blank_comments(&src));
    scan_source(&relative, &src, scan);
}

fn walk(root: &Path, dir: &Path, scan: &mut Scan, files: &mut usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("tests") {
                continue;
            }
            walk(root, &path, scan, files);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            *files += 1;
            scan_file(root, &path, scan);
        }
    }
}

/// The checker itself: a raw connection string is caught in every form a
/// site can take, and a redacted one is not. Without this, a checker that
/// matched nothing would pass the corpus below.
#[test]
fn checker_catches_every_form_of_a_raw_connection_string() {
    let raw = r#"
        fn f() {
            info!(url = %url, "Initializing");
            tracing::warn!(%database_url, "x");
            debug!(endpoint_url = %self.url, "y");
            info!("Configured gateway: {}", resolved_url);
            error!("connect to {dsn} failed");
            info_span!("s", target_endpoint = ?cfg.endpoint);
        }
        #[instrument(skip(self), fields(repo = %binding.repo_url))]
        async fn g(&self, binding: &B) {}
        #[instrument(skip(self))]
        async fn h(&self, connection_string: &str) {}
    "#;
    let mut scan = Scan {
        violations: Vec::new(),
        sites: 0,
    };
    scan_source("orchestrator/core/src/example.rs", raw, &mut scan);
    assert_eq!(
        scan.violations.len(),
        8,
        "every raw form must be reported once:\n{}",
        scan.violations.join("\n")
    );

    let redacted = r#"
        fn f() {
            info!(url = %url.redacted(), "Initializing");
            tracing::warn!(url = %SensitiveUrl::new(raw.clone()), "x");
            info!("Fetch: {}", sanitize_url(&url));
            info!(host = %host, port = port, "no connection string here");
        }
        #[instrument(skip(self, connection_string))]
        async fn h(&self, connection_string: &str) {}
        // info!(url = %url, "a comment is not output");
        #[cfg(test)]
        mod tests {
            fn t() { info!(url = %url, "a test is not the daemon"); }
        }
    "#;
    let mut scan = Scan {
        violations: Vec::new(),
        sites: 0,
    };
    let src = blank_test_modules(&blank_comments(redacted));
    scan_source("orchestrator/core/src/example.rs", &src, &mut scan);
    assert!(
        scan.violations.is_empty(),
        "redacted, commented and test-only forms must pass:\n{}",
        scan.violations.join("\n")
    );
}

#[test]
fn no_output_site_renders_a_raw_connection_string() {
    let root = workspace_root();
    let mut scan = Scan {
        violations: Vec::new(),
        sites: 0,
    };
    let mut files = 0;
    for crate_root in ROOTS {
        walk(&root, &root.join(crate_root), &mut scan, &mut files);
    }
    // A walk that found nothing would pass vacuously.
    assert!(
        files > 200 && scan.sites > 1000,
        "precondition: the walk reached {files} files and {} output sites; the workspace has far more",
        scan.sites
    );
    assert!(
        scan.violations.is_empty(),
        "{} output site(s) render a raw connection string, which can carry a credential. \
         Hold the value in `SensitiveUrl` (aegis_orchestrator_core::domain::secrets) and render \
         it with `%url` via `.redacted()`, or pass it through `sanitize_url`:\n{}",
        scan.violations.len(),
        scan.violations.join("\n")
    );
}
