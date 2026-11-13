//! Tiny JSON writer used by `--json` output.
//!
//! The crate intentionally has no dependencies, so this module implements the
//! small subset of JSON needed for diagnostics: null, booleans, integers,
//! strings, arrays and objects.  Strings are escaped per RFC 8259.

/// A JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// A signed integer.
    Int(i64),
    /// An unsigned integer.
    UInt(u64),
    /// A string.
    Str(String),
    /// An array.
    Array(Vec<Json>),
    /// An object; order is preserved.
    Object(Vec<(String, Json)>),
}

impl Json {
    /// Create a string value.
    pub fn str(value: impl Into<String>) -> Self {
        Self::Str(value.into())
    }

    /// Create a string value, or `null` when absent.
    pub fn opt_str(value: Option<impl Into<String>>) -> Self {
        match value {
            Some(value) => Self::Str(value.into()),
            None => Self::Null,
        }
    }

    /// Create an object value.
    pub fn object(entries: impl IntoIterator<Item = (impl Into<String>, Json)>) -> Self {
        Self::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
        )
    }

    /// Create an array value.
    pub fn array(items: impl IntoIterator<Item = Json>) -> Self {
        Self::Array(items.into_iter().collect())
    }

    /// Render the value as indented JSON, without a trailing newline.
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.write_pretty(&mut out, 0);
        out
    }

    fn write_pretty(&self, out: &mut String, indent: usize) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(true) => out.push_str("true"),
            Self::Bool(false) => out.push_str("false"),
            Self::Int(value) => out.push_str(&value.to_string()),
            Self::UInt(value) => out.push_str(&value.to_string()),
            Self::Str(value) => write_string(out, value),
            Self::Array(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push_str("[\n");
                for (index, item) in items.iter().enumerate() {
                    push_indent(out, indent + 1);
                    item.write_pretty(out, indent + 1);
                    if index + 1 < items.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                push_indent(out, indent);
                out.push(']');
            }
            Self::Object(entries) => {
                if entries.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push_str("{\n");
                for (index, (key, value)) in entries.iter().enumerate() {
                    push_indent(out, indent + 1);
                    write_string(out, key);
                    out.push_str(": ");
                    value.write_pretty(out, indent + 1);
                    if index + 1 < entries.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                push_indent(out, indent);
                out.push('}');
            }
        }
    }
}

fn push_indent(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}

fn write_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ch if (ch as u32) < 0x20 => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_nested_values() {
        let value = Json::object([
            ("name", Json::str("PCM-A10")),
            ("ok", Json::Bool(true)),
            ("count", Json::UInt(3)),
            ("missing", Json::Null),
            ("items", Json::array([Json::Int(-1), Json::str("two")])),
        ]);
        let text = value.render();
        assert!(text.contains("\"name\": \"PCM-A10\""));
        assert!(text.contains("\"ok\": true"));
        assert!(text.contains("\"count\": 3"));
        assert!(text.contains("\"missing\": null"));
        assert!(text.contains("\"items\": ["));
        assert!(text.contains("-1,"));
    }

    #[test]
    fn escapes_strings() {
        let text = Json::str("a\"b\\c\n\t\u{1}").render();
        assert_eq!(text, "\"a\\\"b\\\\c\\n\\t\\u0001\"");
    }

    #[test]
    fn empty_containers_are_compact() {
        assert_eq!(Json::Array(vec![]).render(), "[]");
        assert_eq!(Json::Object(vec![]).render(), "{}");
    }
}
