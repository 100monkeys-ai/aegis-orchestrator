// Copyright 2026 100monkeys AI, Inc.
// SPDX-License-Identifier: Apache-2.0
//
// Carried from `zaru-cli` commit f9deca3dd08c95b70c5bc8c12693b002f4dedfca,
// file `crates/zaru-cli/src/tools/codebase.rs`, with its four tests (AEGIS
// ADR-145 S8). Changed only where this crate differs: the module link below
// names `fs.search`'s module here, and `stem` spells the source's two
// `if let ... &&` chains as nested `if`s, because this workspace builds on
// edition 2021, whose rustfmt also lays out one test's assertion differently.
// A fix made here is not a fix in `zaru-cli`, nor the reverse.

//! Parser-backed code retrieval for [`super::builtin_search`].
//!
//! Every structural fact here comes from a Tree-sitter concrete syntax tree.
//! A source file that does not parse is not given a guessed declaration.

use std::path::Path;
use tree_sitter::{Language, Node, Parser};

/// A citable declaration, import, or reference from a concrete syntax tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Symbol {
    /// The path as the caller named it to [`collect`].
    pub(crate) path: String,
    /// The line it starts on, from 1.
    pub(crate) line: usize,
    /// What it is: `function`, `class`, `import`, `reference` and so on.
    pub(crate) kind: &'static str,
    /// Its name.
    pub(crate) name: String,
    /// The names it is declared inside, joined by `::`.
    pub(crate) scope: String,
    /// The comment lines just above it and the start of its text.
    pub(crate) context: String,
}

impl Symbol {
    /// Whether this declares something, rather than importing or using it.
    pub(crate) fn is_declaration(&self) -> bool {
        !matches!(self.kind, "import" | "reference")
    }

    /// One row of an answer: its line, what it is, its name, where it is
    /// declared, and its context.
    pub(crate) fn row(&self) -> String {
        let scope = (!self.scope.is_empty()).then(|| format!(" in {}", self.scope));
        let context: String = self.context.trim().chars().take(160).collect();
        format!(
            "{}: {} {}{} — {}",
            self.line,
            self.kind,
            self.name,
            scope.unwrap_or_default(),
            context
        )
    }
}

/// Parse one supported file and collect only tree-derived facts.
pub(crate) fn collect(path: &Path, text: &str, symbols: &mut Vec<Symbol>) {
    let Some(language) = language_for(path) else {
        return;
    };
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return;
    }
    let Some(tree) = parser.parse(text, None) else {
        return;
    };
    if tree.root_node().has_error() {
        return;
    }
    visit(tree.root_node(), text, path, "", symbols);
}

/// The declarations that share the most words with `query`, best first.
///
/// Until 2026-09-28 every word of the query had to name the same symbol as a
/// whole word, and uses and imports were offered beside declarations, so
/// "where is the retry logic" found nothing and a common name returned uses.
/// Now the question's small words are dropped, a word matches any form of
/// itself (`encrypted` matches `encrypts`), at least half the remaining words
/// must match, and only declarations are offered.
pub(crate) fn retrieve<'a>(symbols: &'a [Symbol], query: &str) -> Vec<&'a Symbol> {
    let query = query_terms(query);
    if query.is_empty() {
        return Vec::new();
    }
    let mut ranked: Vec<(usize, &Symbol)> = symbols
        .iter()
        .filter(|symbol| symbol.is_declaration())
        .filter_map(|symbol| score(symbol, &query).map(|score| (score, symbol)))
        .collect();
    ranked.sort_by(|(ls, left), (rs, right)| {
        rs.cmp(ls)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.name.cmp(&right.name))
    });
    ranked.into_iter().map(|(_, symbol)| symbol).collect()
}

/// The words of a query that carry meaning: split at case changes and
/// punctuation, lowercased, and without the small words of a question.
pub(crate) fn query_terms(query: &str) -> Vec<String> {
    const SMALL: [&str; 35] = [
        "a", "an", "and", "any", "are", "as", "at", "be", "by", "code", "do", "does", "for",
        "from", "handled", "how", "in", "into", "is", "it", "its", "of", "on", "or", "the", "this",
        "that", "to", "what", "when", "where", "which", "who", "why", "with",
    ];
    terms(query)
        .into_iter()
        .filter(|term| !SMALL.contains(&term.as_str()))
        .collect()
}

fn language_for(path: &Path) -> Option<Language> {
    Some(match path.extension()?.to_str()? {
        "rs" => tree_sitter_rust::LANGUAGE.into(),
        "py" => tree_sitter_python::LANGUAGE.into(),
        "js" | "jsx" => tree_sitter_javascript::LANGUAGE.into(),
        "ts" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        _ => return None,
    })
}

fn visit(node: Node<'_>, text: &str, path: &Path, scope: &str, symbols: &mut Vec<Symbol>) {
    let kind = node_kind(node.kind());
    // An `impl` block has no name; the type it is for scopes its methods.
    let name = node
        .child_by_field_name("name")
        .or_else(|| {
            (node.kind() == "impl_item")
                .then(|| node.child_by_field_name("type"))
                .flatten()
        })
        .and_then(|node| node_text(node, text));
    let next_scope = if let (Some(kind), Some(name)) = (kind, name) {
        push(path, node, kind, name, scope, text, symbols);
        if matches!(kind, "import" | "reference") {
            scope.to_owned()
        } else {
            join_scope(scope, name)
        }
    } else if is_import(node.kind()) {
        let import = compact(node_text(node, text).unwrap_or_default());
        if !import.is_empty() {
            push(path, node, "import", &import, scope, text, symbols);
        }
        scope.to_owned()
    } else if node.kind() == "identifier" && is_reference(node) {
        if let Some(name) = node_text(node, text) {
            push(path, node, "reference", name, scope, text, symbols);
        }
        scope.to_owned()
    } else {
        scope.to_owned()
    };
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, text, path, &next_scope, symbols);
    }
}

fn node_kind(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "function_item"
        | "function_definition"
        | "function_declaration"
        | "method_declaration"
        | "method_definition" => "function",
        "struct_item" | "class_definition" | "class_declaration" => "class",
        "enum_item" | "enum_declaration" => "enum",
        "trait_item" | "interface_declaration" => "interface",
        "impl_item" => "implementation",
        "mod_item" | "module" => "module",
        "type_item" | "type_alias_declaration" | "type_declaration" => "type",
        "const_item" => "constant",
        _ => return None,
    })
}

fn is_import(kind: &str) -> bool {
    matches!(
        kind,
        "use_declaration"
            | "import_statement"
            | "import_from_statement"
            | "import_declaration"
            | "package_clause"
    )
}
fn is_reference(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    parent
        .child_by_field_name("name")
        .is_none_or(|name| name.id() != node.id())
}
fn push(
    path: &Path,
    node: Node<'_>,
    kind: &'static str,
    name: &str,
    scope: &str,
    text: &str,
    symbols: &mut Vec<Symbol>,
) {
    symbols.push(Symbol {
        path: path.display().to_string(),
        line: node.start_position().row + 1,
        kind,
        name: name.to_owned(),
        scope: scope.to_owned(),
        context: excerpt(node, text),
    });
}
fn node_text<'a>(node: Node<'_>, text: &'a str) -> Option<&'a str> {
    text.get(node.byte_range())
}
fn join_scope(scope: &str, name: &str) -> String {
    if scope.is_empty() {
        name.to_owned()
    } else {
        format!("{scope}::{name}")
    }
}
fn compact(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn excerpt(node: Node<'_>, text: &str) -> String {
    // The comment block just above, from its first line: a doc comment's
    // summary comes first. At most six lines of it. Read backwards from the
    // node, so the cost is the block's and not the file's.
    let before = text.get(..node.start_byte()).unwrap_or_default();
    let above = before.rfind('\n').map_or("", |at| &before[..at]);
    let mut leading: Vec<&str> = above
        .lines()
        .rev()
        .map(str::trim)
        .take_while(|line| {
            ["//", "#", "*", "/*", "\"\"\"", "--"]
                .iter()
                .any(|mark| line.starts_with(mark))
        })
        .take(40)
        .collect();
    leading.reverse();
    leading.truncate(6);
    format!(
        "{} {}",
        leading.join(" "),
        node_text(node, text).map(compact).unwrap_or_default()
    )
    .chars()
    .take(240)
    .collect()
}
/// How well `symbol` answers `query`: `None` when fewer than half the words
/// match, or when none matches its name, scope or comment.
fn score(symbol: &Symbol, query: &[String]) -> Option<usize> {
    let name = terms(&symbol.name);
    let scope = terms(&symbol.scope);
    let context = terms(&symbol.context);
    let path = terms(&symbol.path);
    let has = |words: &[String], term: &str| words.iter().any(|word| same_word(word, term));
    let mut score = 0;
    let mut matched = 0;
    let mut in_the_symbol = false;
    for term in query {
        let points = if has(&name, term) {
            16
        } else if has(&scope, term) {
            10
        } else if has(&context, term) {
            5
        } else if has(&path, term) {
            2
        } else {
            0
        };
        if points > 0 {
            matched += 1;
            in_the_symbol |= points > 2;
        }
        score += points;
    }
    (in_the_symbol && matched * 2 >= query.len()).then_some(score)
}
/// Whether two lowercase words are forms of one word: equal once a common
/// ending is taken off, or one a start of the other of five letters or more.
fn same_word(left: &str, right: &str) -> bool {
    let (left, right) = (stem(left), stem(right));
    if left == right {
        return true;
    }
    let (short, long) = if left.len() <= right.len() {
        (left, right)
    } else {
        (right, left)
    };
    short.len() >= 5 && long.starts_with(short)
}
/// A word without its commonest English ending.
fn stem(word: &str) -> &str {
    if let Some(root) = word.strip_suffix("ies") {
        if root.len() >= 3 {
            return root;
        }
    }
    for ending in ["ing", "ions", "ion", "ed", "es", "er", "s", "e", "y"] {
        if let Some(root) = word.strip_suffix(ending) {
            if root.len() >= 4 || (ending == "s" && root.len() >= 3) {
                return root;
            }
        }
    }
    word
}
fn terms(value: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut previous_lower = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            if character.is_ascii_uppercase() && previous_lower && !word.is_empty() {
                words.push(word.to_ascii_lowercase());
                word.clear();
            }
            previous_lower = character.is_ascii_lowercase();
            word.push(character);
        } else {
            if !word.is_empty() {
                words.push(word.to_ascii_lowercase());
                word.clear();
            }
            previous_lower = false;
        }
    }
    if !word.is_empty() {
        words.push(word.to_ascii_lowercase());
    }
    words.sort();
    words.dedup();
    words
}

#[cfg(test)]
mod tests {
    use super::{collect, query_terms, retrieve, same_word};
    use std::path::Path;
    #[test]
    fn tree_sitter_returns_a_scoped_rust_declaration_and_keeps_its_uses_out_of_retrieval() {
        let mut symbols = Vec::new();
        collect(
            Path::new("src/clock.rs"),
            "mod turn_clock { pub fn refreshTurnClock() { refreshTurnClock(); } }",
            &mut symbols,
        );
        assert!(
            symbols.iter().any(|symbol| symbol.kind == "reference"),
            "the use was not collected: {symbols:?}"
        );
        let hit: Vec<String> = retrieve(&symbols, "turn_clock refresh")
            .iter()
            .map(|symbol| symbol.row())
            .collect();
        let hit = hit.join("\n");
        assert!(
            hit.contains("function refreshTurnClock in turn_clock"),
            "{hit}"
        );
        assert!(!hit.contains("reference"), "a use was retrieved: {hit}");
    }
    #[test]
    fn a_syntax_error_is_not_promoted_to_a_guessed_symbol() {
        let mut symbols = Vec::new();
        collect(
            Path::new("broken.rs"),
            "fn not actually valid(",
            &mut symbols,
        );
        assert!(symbols.is_empty());
    }
    #[test]
    fn typescript_is_parsed_by_its_own_grammar() {
        let mut symbols = Vec::new();
        collect(
            Path::new("view.ts"),
            "export interface TurnClock { refreshTurnClock(): void }",
            &mut symbols,
        );
        assert!(retrieve(&symbols, "turn clock")
            .iter()
            .any(|symbol| symbol.row().contains("interface TurnClock")));
    }
    /// Half the words are enough, in any form of the word, and the small
    /// words of a question do not count.
    ///
    /// Red on the unfixed tree: every word had to match whole, so "where is
    /// the retry logic" found nothing.
    #[test]
    fn half_the_words_in_any_form_find_a_declaration() {
        assert_eq!(query_terms("where is the retry logic"), ["logic", "retry"]);
        for (left, right) in [
            ("retry", "retries"),
            ("encrypted", "encrypts"),
            ("throttle", "throttling"),
            ("key", "keys"),
            ("config", "configuration"),
        ] {
            assert!(same_word(left, right), "{left} and {right} are one word");
        }
        assert!(!same_word("seal", "search"));
        let mut symbols = Vec::new();
        collect(
            Path::new("src/net.rs"),
            "/// Tries again after a pause.\npub fn retry_request() {}\npub fn other() {}\n",
            &mut symbols,
        );
        let found = retrieve(&symbols, "where is the retry logic");
        assert_eq!(
            found.first().map(|symbol| symbol.name.as_str()),
            Some("retry_request"),
            "{found:?}"
        );
    }
}
