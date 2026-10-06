// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! No text a person or a model receives names a decision record (AEGIS
//! ADR-134 D1, D3).
//!
//! The facing set, as the survey `facing-text-no-adr-refs` of 2026-10-06
//! defines it:
//!
//! - every string literal in non-test Rust under `cli/`, `orchestrator/` and
//!   `sdks/` (`orchestrator/vendor/` is third-party and out);
//! - every doc comment inside an item that derives `JsonSchema` (schemars turns
//!   it into a schema description `schema.get` returns) or a clap `Parser`,
//!   `Args`, `Subcommand` or `ValueEnum` (clap turns it into help text);
//! - every string scalar of the built-in agent and workflow templates
//!   (`cli/templates/agents/`, `cli/templates/workflows/`), and the non-comment
//!   lines of the service templates (`cli/templates/*.service`, `*.plist`,
//!   `*.json`).
//!
//! Comments are out (D2), and so is test code: `tests/` and `benches/`
//! directories, files named `tests.rs`, `*_tests.rs` or `*_test.rs`, files a
//! `#[cfg(test)] mod x;` declares, and `#[cfg(test)]` items and `#[test]`
//! functions with any attributes between the `cfg` and the item. One exclusion
//! (the ruling of batch 762, generated-config comments stay): a line of a
//! string literal under `cli/src/commands/init/` whose first non-blank
//! character is `#` is a comment of the config file `aegis init` writes.
//!
//! A second test runs the same pattern over the JSON schemas the schema
//! registry builds, which is what `aegis.schema.get` returns.

use aegis_orchestrator_core::application::schema_registry::SchemaRegistry;
use regex::Regex;
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

/// The record pattern of D3: records (`ADR-`, `CD-`), the security audit's
/// citations and the library's section numbers.
fn pattern() -> Regex {
    Regex::new(
        r"ADR-[0-9]|CD-[0-9]|ADR [0-9]|(?i:decision record)|security audit 0|audit 0[0-9][0-9]|§[0-9]",
    )
    .expect("the record pattern compiles")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root resolves")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Region {
    Code,
    Str,
    LineComment,
    DocComment,
    BlockComment,
}

/// One Rust source file, lexed into a region per character.
struct Lexed {
    chars: Vec<char>,
    region: Vec<Region>,
    line_of: Vec<usize>,
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn lex(src: &str) -> Lexed {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut region = vec![Region::Code; n];
    let mut line_of = Vec::with_capacity(n);
    let mut line = 1;
    for &c in &chars {
        line_of.push(line);
        if c == '\n' {
            line += 1;
        }
    }
    let starts = |i: usize, s: &str| -> bool {
        let s: Vec<char> = s.chars().collect();
        i + s.len() <= n && chars[i..i + s.len()] == s[..]
    };
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if starts(i, "//") {
            let doc = (starts(i, "///") && !starts(i, "////")) || starts(i, "//!");
            let mut j = i;
            while j < n && chars[j] != '\n' {
                j += 1;
            }
            let r = if doc {
                Region::DocComment
            } else {
                Region::LineComment
            };
            region[i..j].fill(r);
            i = j;
            continue;
        }
        if starts(i, "/*") {
            let doc =
                (starts(i, "/**") && !starts(i, "/***") && !starts(i, "/**/")) || starts(i, "/*!");
            let mut depth = 1;
            let mut j = i + 2;
            while j < n && depth > 0 {
                if starts(j, "/*") {
                    depth += 1;
                    j += 2;
                } else if starts(j, "*/") {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            let r = if doc {
                Region::DocComment
            } else {
                Region::BlockComment
            };
            region[i..j].fill(r);
            i = j;
            continue;
        }
        let prev_ident = i > 0 && is_ident(chars[i - 1]);
        // Raw strings: r"…", r#"…"#, br#"…"#.
        if !prev_ident && (c == 'r' || (c == 'b' && i + 1 < n && chars[i + 1] == 'r')) {
            let mut k = if c == 'b' { i + 2 } else { i + 1 };
            let mut hashes = 0;
            while k < n && chars[k] == '#' {
                hashes += 1;
                k += 1;
            }
            if k < n && chars[k] == '"' {
                let mut j = k + 1;
                loop {
                    if j >= n {
                        break;
                    }
                    if chars[j] == '"'
                        && (0..hashes).all(|h| j + 1 + h < n && chars[j + 1 + h] == '#')
                    {
                        j += 1 + hashes;
                        break;
                    }
                    j += 1;
                }
                region[i..j.min(n)].fill(Region::Str);
                i = j;
                continue;
            }
        }
        if c == '"' {
            let mut j = i + 1;
            while j < n && chars[j] != '"' {
                j += if chars[j] == '\\' { 2 } else { 1 };
            }
            let end = (j + 1).min(n);
            region[i..end].fill(Region::Str);
            i = end;
            continue;
        }
        if c == '\'' {
            // A char literal ('x', '\n', '\u{1F600}', '"') or a lifetime ('a).
            if i + 1 < n && chars[i + 1] == '\\' {
                let mut j = i + 2;
                while j < n && chars[j] != '\'' {
                    j += 1;
                }
                i = j + 1;
                continue;
            }
            if i + 2 < n && chars[i + 2] == '\'' {
                i += 3;
                continue;
            }
            i += 1;
            continue;
        }
        i += 1;
    }
    Lexed {
        chars,
        region,
        line_of,
    }
}

impl Lexed {
    fn code_at(&self, i: usize, s: &str) -> bool {
        let s: Vec<char> = s.chars().collect();
        i + s.len() <= self.chars.len()
            && self.chars[i..i + s.len()] == s[..]
            && self.region[i..i + s.len()]
                .iter()
                .all(|r| *r == Region::Code)
    }

    fn skip_space(&self, mut i: usize) -> usize {
        while i < self.chars.len()
            && (self.chars[i].is_whitespace() || self.region[i] != Region::Code)
        {
            i += 1;
        }
        i
    }

    /// The index after the bracket matching the one at `open`.
    fn match_bracket(&self, open: usize, o: char, c: char) -> usize {
        let mut depth = 0;
        let mut i = open;
        while i < self.chars.len() {
            if self.region[i] == Region::Code {
                if self.chars[i] == o {
                    depth += 1;
                } else if self.chars[i] == c {
                    depth -= 1;
                    if depth == 0 {
                        return i + 1;
                    }
                }
            }
            i += 1;
        }
        self.chars.len()
    }

    /// Skip attributes (`#[…]`) and whitespace from `i`.
    fn skip_attrs(&self, mut i: usize) -> usize {
        loop {
            i = self.skip_space(i);
            if self.code_at(i, "#[") {
                i = self.match_bracket(i + 1, '[', ']');
            } else {
                return i;
            }
        }
    }

    fn word_at(&self, i: usize) -> String {
        let mut j = i;
        while j < self.chars.len() && is_ident(self.chars[j]) {
            j += 1;
        }
        self.chars[i..j].iter().collect()
    }

    /// The end of the item starting at `i`: its first `;` or the end of its
    /// first brace block at bracket depth zero, whichever comes first.
    fn item_end(&self, i: usize) -> usize {
        let mut depth = 0i32;
        let mut j = i;
        while j < self.chars.len() {
            if self.region[j] == Region::Code {
                match self.chars[j] {
                    '(' | '[' => depth += 1,
                    ')' | ']' => depth -= 1,
                    ';' if depth == 0 => return j + 1,
                    '{' if depth == 0 => return self.match_bracket(j, '{', '}'),
                    _ => {}
                }
            }
            j += 1;
        }
        self.chars.len()
    }
}

/// Test regions of one file, and the files its `#[cfg(test)] mod x;` names.
fn test_regions(lx: &Lexed) -> (Vec<(usize, usize)>, Vec<String>) {
    let mut regions = Vec::new();
    let mut test_mods = Vec::new();
    let n = lx.chars.len();
    let mut i = 0;
    while i < n {
        if lx.code_at(i, "#[") {
            let close = lx.match_bracket(i + 1, '[', ']');
            let inner: String = lx.chars[i + 2..close.saturating_sub(1)]
                .iter()
                .collect::<String>()
                .split_whitespace()
                .collect();
            let is_cfg_test = inner == "cfg(test)";
            let is_test_fn = inner == "test"
                || inner.ends_with("::test")
                || inner.starts_with("test(")
                || (inner.contains("::test(") && !inner.starts_with("cfg"));
            if is_cfg_test || is_test_fn {
                let mut k = lx.skip_attrs(close);
                for kw in ["pub(crate)", "pub(super)", "pub"] {
                    if lx.code_at(k, kw) {
                        k = lx.skip_space(k + kw.len());
                        break;
                    }
                }
                if lx.word_at(k) == "mod" {
                    let name_at = lx.skip_space(k + 3);
                    let name = lx.word_at(name_at);
                    let after = lx.skip_space(name_at + name.len());
                    if after < n && lx.chars[after] == ';' {
                        test_mods.push(name);
                    }
                }
                let end = lx.item_end(k);
                regions.push((i, end));
                i = end;
                continue;
            }
            i = close;
            continue;
        }
        i += 1;
    }
    (regions, test_mods)
}

/// Spans of items deriving `JsonSchema` or a clap derive, from the doc and
/// attribute lines above the derive to the end of the item.
fn facing_doc_spans(lx: &Lexed) -> Vec<(usize, usize)> {
    const FACING: [&str; 5] = ["JsonSchema", "Parser", "Args", "Subcommand", "ValueEnum"];
    let mut spans = Vec::new();
    let n = lx.chars.len();
    let mut i = 0;
    while i < n {
        if lx.code_at(i, "#[derive(") {
            let close = lx.match_bracket(i + 8, '(', ')');
            let list: String = lx.chars[i + 9..close.saturating_sub(1)].iter().collect();
            let facing = list.split(',').any(|d| {
                let d = d.trim();
                FACING.contains(&d.rsplit("::").next().unwrap_or(d))
            });
            if facing {
                // Walk back over the doc and attribute lines above the derive.
                let mut start = i;
                while start > 0 && lx.chars[start - 1] != '\n' {
                    start -= 1;
                }
                loop {
                    if start == 0 {
                        break;
                    }
                    let mut prev = start - 1;
                    while prev > 0 && lx.chars[prev - 1] != '\n' {
                        prev -= 1;
                    }
                    let text: String = lx.chars[prev..start].iter().collect();
                    let t = text.trim_start();
                    if t.starts_with("///") || t.starts_with("#[") || t.starts_with("/**") {
                        start = prev;
                    } else {
                        break;
                    }
                }
                let item = lx.skip_attrs(close + 1);
                let end = lx.item_end(item);
                spans.push((start, end));
            }
            i = close;
            continue;
        }
        i += 1;
    }
    spans
}

fn in_any(spans: &[(usize, usize)], i: usize) -> bool {
    spans.iter().any(|(s, e)| *s <= i && i < *e)
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

fn is_test_path(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    rel.split('/').any(|seg| seg == "tests" || seg == "benches")
        || name == "tests.rs"
        || name.ends_with("_tests.rs")
        || name.ends_with("_test.rs")
}

fn rust_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for top in ["cli", "orchestrator", "sdks"] {
        for entry in walkdir::WalkDir::new(root.join(top))
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                !(e.file_type().is_dir()
                    && (name == "target" || name == "worktrees" || name == "vendor"))
            })
            .filter_map(Result::ok)
        {
            if entry.file_type().is_file() && entry.path().extension().is_some_and(|x| x == "rs") {
                out.push(entry.path().to_path_buf());
            }
        }
    }
    out.sort();
    out
}

/// Every offending facing row in non-test Rust, as `file:line: text`.
fn rust_rows(root: &Path, pat: &Regex) -> BTreeSet<String> {
    let files = rust_files(root);
    let mut lexed = Vec::new();
    let mut excluded: HashSet<PathBuf> = HashSet::new();
    for p in &files {
        let src = std::fs::read_to_string(p).expect("a source file reads");
        let lx = lex(&src);
        let (regions, mods) = test_regions(&lx);
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let dir = if ["mod.rs", "lib.rs", "main.rs"].contains(&name.as_str()) {
            p.parent().unwrap().to_path_buf()
        } else {
            p.with_extension("")
        };
        for m in mods {
            excluded.insert(dir.join(format!("{m}.rs")));
            excluded.insert(dir.join(&m).join("mod.rs"));
        }
        lexed.push((p.clone(), src, lx, regions));
    }
    let mut rows = BTreeSet::new();
    for (p, src, lx, tests) in lexed {
        let r = rel(root, &p);
        if is_test_path(&r) || excluded.contains(&p) {
            continue;
        }
        let docs = facing_doc_spans(&lx);
        let lines: Vec<&str> = src.split('\n').collect();
        let init = r.starts_with("cli/src/commands/init/");
        // Per line, the facing text on it.
        let mut facing: Vec<String> = vec![String::new(); lines.len() + 1];
        for (i, ch) in lx.chars.iter().enumerate() {
            if in_any(&tests, i) {
                continue;
            }
            let keep = match lx.region[i] {
                Region::Str => true,
                Region::DocComment => in_any(&docs, i),
                _ => false,
            };
            if keep {
                facing[lx.line_of[i]].push(*ch);
            }
        }
        for (ln, text) in facing.iter().enumerate() {
            if text.is_empty() || !pat.is_match(text) {
                continue;
            }
            let line = lines[ln - 1];
            if init && line.trim_start().starts_with('#') {
                continue;
            }
            rows.insert(format!("{r}:{ln}: {}", line.trim()));
        }
    }
    rows
}

fn yaml_strings(v: &serde_yaml::Value, out: &mut Vec<String>) {
    match v {
        serde_yaml::Value::String(s) => out.push(s.clone()),
        serde_yaml::Value::Sequence(seq) => seq.iter().for_each(|x| yaml_strings(x, out)),
        serde_yaml::Value::Mapping(m) => {
            for (k, x) in m {
                yaml_strings(k, out);
                yaml_strings(x, out);
            }
        }
        serde_yaml::Value::Tagged(t) => yaml_strings(&t.value, out),
        _ => {}
    }
}

/// Every offending row of the built-in and service templates.
fn template_rows(root: &Path, pat: &Regex) -> BTreeSet<String> {
    let mut rows = BTreeSet::new();
    let templates = root.join("cli/templates");
    for dir in ["agents", "workflows"] {
        let mut files: Vec<PathBuf> = std::fs::read_dir(templates.join(dir))
            .expect("a template directory reads")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "yaml" || x == "yml"))
            .collect();
        files.sort();
        for p in files {
            let raw = std::fs::read_to_string(&p).expect("a template reads");
            let value: serde_yaml::Value = serde_yaml::from_str(&raw).expect("a template parses");
            let mut strings = Vec::new();
            yaml_strings(&value, &mut strings);
            let raw_lines: Vec<&str> = raw.split('\n').collect();
            for s in strings {
                for piece in s.split('\n').filter(|l| pat.is_match(l)) {
                    let piece = piece.trim();
                    let ln = raw_lines
                        .iter()
                        .position(|l| l.contains(piece))
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    rows.insert(format!("{}:{ln}: {piece}", rel(root, &p)));
                }
            }
        }
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&templates)
        .expect("the template directory reads")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .is_some_and(|x| x == "service" || x == "plist" || x == "json")
        })
        .collect();
    files.sort();
    for p in files {
        let raw = std::fs::read_to_string(&p).expect("a template reads");
        let mut in_xml_comment = false;
        for (i, line) in raw.split('\n').enumerate() {
            let t = line.trim_start();
            let mut text = line.to_string();
            if p.extension().is_some_and(|x| x == "service")
                && (t.starts_with('#') || t.starts_with(';'))
            {
                continue;
            }
            if p.extension().is_some_and(|x| x == "plist") {
                // Drop XML comments, which may span lines.
                let mut kept = String::new();
                let mut rest = line;
                loop {
                    if in_xml_comment {
                        match rest.find("-->") {
                            Some(e) => {
                                in_xml_comment = false;
                                rest = &rest[e + 3..];
                            }
                            None => break,
                        }
                    } else {
                        match rest.find("<!--") {
                            Some(s) => {
                                kept.push_str(&rest[..s]);
                                in_xml_comment = true;
                                rest = &rest[s + 4..];
                            }
                            None => {
                                kept.push_str(rest);
                                break;
                            }
                        }
                    }
                }
                text = kept;
            }
            if pat.is_match(&text) {
                rows.insert(format!("{}:{}: {}", rel(root, &p), i + 1, line.trim()));
            }
        }
    }
    rows
}

/// D3: no facing string in the repository names a record.
#[test]
fn no_facing_string_names_a_decision_record() {
    let root = repo_root();
    let pat = pattern();
    let mut rows = rust_rows(&root, &pat);
    rows.extend(template_rows(&root, &pat));
    for row in &rows {
        println!("{row}");
    }
    assert!(
        rows.is_empty(),
        "{} facing strings name a decision record, a security audit or a library section:\n{}",
        rows.len(),
        rows.iter().cloned().collect::<Vec<_>>().join("\n")
    );
}

/// D3, as a model reads it: the schemas `aegis.schema.get` returns name no
/// record.
#[test]
fn manifest_schemas_name_no_decision_record() {
    let pat = pattern();
    let registry = SchemaRegistry::build();
    let mut found = Vec::new();
    for key in ["agent/manifest/v1", "workflow/manifest/v1"] {
        let schema = registry.get(key).expect("the schema is registered");
        let text = serde_json::to_string(schema).expect("the schema serialises");
        for m in pat.find_iter(&text) {
            let from = text[..m.start()]
                .char_indices()
                .rev()
                .nth(60)
                .map(|(i, _)| i)
                .unwrap_or(0);
            let to = (m.end() + 40).min(text.len());
            let to = (to..=text.len())
                .find(|i| text.is_char_boundary(*i))
                .unwrap_or(text.len());
            found.push(format!("{key}: …{}…", &text[from..to]));
        }
    }
    for f in &found {
        println!("{f}");
    }
    assert!(
        found.is_empty(),
        "{} record references in the manifest schemas:\n{}",
        found.len(),
        found.join("\n")
    );
}
