//! The string and value rules of the Python generator this program replaced,
//! kept exact so the generated files stay byte for byte what they were:
//! `str.strip`, `str.splitlines`, truthiness, and `repr` in error messages.

use serde_json::{Map, Value};

pub type Obj = Map<String, Value>;

/// `str.isspace()`: Unicode whitespace plus the four ASCII separators
/// U+001C..U+001F that Python counts and Rust does not.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

pub fn strip(text: &str) -> &str {
    text.trim_matches(is_space)
}

pub fn rstrip(text: &str) -> &str {
    text.trim_end_matches(is_space)
}

/// `str.splitlines()`: every Python line boundary, no empty last line.
pub fn splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        let boundary = matches!(
            c,
            '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}'
        );
        if !boundary {
            continue;
        }
        lines.push(&text[start..index]);
        let mut end = index + c.len_utf8();
        if c == '\r'
            && let Some((next, '\n')) = chars.peek().copied()
        {
            chars.next();
            end = next + 1;
        }
        start = end;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// Text-mode reads translate `\r\n` and a lone `\r` to `\n`.
pub fn universal_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Python truthiness of an optional JSON value: what `x or y` and `if x:` test.
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|value| value != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

/// `value or other` for a JSON lookup: the first truthy one, else the last.
pub fn or<'a>(value: Option<&'a Value>, other: Option<&'a Value>) -> Option<&'a Value> {
    if truthy(value) { value } else { other }
}

/// `str.isupper()` for an ASCII word.
pub fn is_upper(word: &str) -> bool {
    word.chars().any(|c| c.is_uppercase()) && !word.chars().any(|c| c.is_lowercase())
}

/// `repr()` of a parsed JSON value.
pub fn repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => repr_str(text),
        Value::Array(items) => format!("[{}]", items.iter().map(repr).collect::<Vec<_>>().join(", ")),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(key, child)| format!("{}: {}", repr_str(key), repr(child)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// `str()` of an optional JSON value, as an f-string prints it: a string bare,
/// a missing value `None`, anything else its `repr`.
pub fn str_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(other) => repr(other),
        None => "None".to_string(),
    }
}

/// `repr()` of a list of strings.
pub fn repr_list<S: AsRef<str>>(items: &[S]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|item| repr_str(item.as_ref()))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// `repr(str)`: single quotes unless the text holds a single quote and no
/// double quote; backslash, the quote and control characters escaped.
pub fn repr_str(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `json.dumps(value, sort_keys=True)` as an identity: equal exactly when
/// Python's two dumps are equal (keys sorted, an int and a float distinct).
pub fn canonical(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

/// [`canonical`] of an object.
pub fn canonical_map(map: &Obj) -> String {
    let mut out = String::new();
    write_canonical_map(map, &mut out);
    out
}

fn write_canonical_map(map: &Obj, out: &mut String) {
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    out.push('{');
    for (index, key) in keys.into_iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&Value::String(key.clone()).to_string());
        out.push(':');
        if let Some(child) = map.get(key) {
            write_canonical(child, out);
        }
    }
    out.push('}');
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => write_canonical_map(map, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Number(number) if number.is_f64() => {
            // `1.0` stays distinct from `1`, as Python's float and int do.
            out.push_str(&format!("f{number}"));
        }
        other => out.push_str(&other.to_string()),
    }
}
