//! Rust names for the document's names.

use std::sync::LazyLock;

use regex::Regex;

use crate::py;

pub const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn",
    "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self",
    "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while", "abstract", "become",
    "box", "do", "final", "macro", "override", "priv", "typeof", "unsized", "virtual", "yield", "try", "gen",
];
const NOT_RAW: &[&str] = &["self", "Self", "crate", "super"];

static LOWER_UPPER: LazyLock<Regex> = LazyLock::new(|| compile(r"([a-z0-9])([A-Z])"));
static UPPER_RUN: LazyLock<Regex> = LazyLock::new(|| compile(r"([A-Z]+)([A-Z][a-z])"));
static SEPARATOR: LazyLock<Regex> = LazyLock::new(|| compile(r"[^0-9A-Za-z]+"));
pub static TYPE_NAME: LazyLock<Regex> = LazyLock::new(|| compile(r"[A-Z][A-Za-z0-9]*"));
pub static OPERATION_CONST: LazyLock<Regex> = LazyLock::new(|| compile(r"pub const ([A-Z0-9_]+): Operation"));

/// The patterns are constants; one that does not compile is a bug in this
/// file, caught by the first run.
fn compile(pattern: &str) -> Regex {
    match Regex::new(pattern) {
        Ok(regex) => regex,
        Err(error) => panic!("generator pattern {pattern:?} does not compile: {error}"),
    }
}

pub fn words(name: &str) -> Vec<String> {
    let name = LOWER_UPPER.replace_all(name, "$1 $2");
    let name = UPPER_RUN.replace_all(&name, "$1 $2");
    SEPARATOR
        .split(&name)
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

fn capitalize_first(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

pub fn pascal(name: &str) -> Result<String, String> {
    let out: String = words(name)
        .iter()
        .map(|word| {
            if py::is_upper(word) && word.chars().count() > 1 {
                let mut chars = word.chars();
                let first: String = chars.next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
                first + &chars.as_str().to_lowercase()
            } else {
                capitalize_first(word)
            }
        })
        .collect();
    if out.is_empty() {
        return Err(format!("cannot name {}", py::repr_str(name)));
    }
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        return Ok(format!("V{out}"));
    }
    Ok(out)
}

pub fn snake(name: &str) -> Result<String, String> {
    let out = words(name)
        .iter()
        .map(|word| word.to_lowercase())
        .collect::<Vec<_>>()
        .join("_");
    if out.is_empty() {
        return Err(format!("cannot name {}", py::repr_str(name)));
    }
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        return Ok(format!("n_{out}"));
    }
    Ok(out)
}

pub fn field_ident(name: &str) -> Result<String, String> {
    let ident = snake(name)?;
    if NOT_RAW.contains(&ident.as_str()) {
        return Ok(format!("{ident}_"));
    }
    if RUST_KEYWORDS.contains(&ident.as_str()) {
        return Ok(format!("r#{ident}"));
    }
    Ok(ident)
}

pub fn bare(ident: &str) -> &str {
    ident.strip_prefix("r#").unwrap_or(ident)
}

pub fn variant_ident(value: &str) -> Result<String, String> {
    let ident = pascal(value)?;
    if RUST_KEYWORDS.contains(&ident.as_str()) {
        return Ok(format!("{ident}_"));
    }
    Ok(ident)
}

/// A Rust string literal: `json.dumps(value, ensure_ascii=False)`, which
/// escapes exactly what serde_json escapes.
pub fn rust_str(value: &str) -> String {
    serde_json::Value::String(value.to_string()).to_string()
}
