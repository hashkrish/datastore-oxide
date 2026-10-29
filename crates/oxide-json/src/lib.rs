//! JSON text that matches what the Java emulator's REST endpoints emit (protobuf-java
//! `JsonFormat` pretty printing):
//!
//! ```text
//! {
//!   "found": [{
//!     "version": "3",
//!     "empty": {
//!     }
//!   }, {
//!     "values": [1, 2]
//!   }]
//! }
//! ```
//!
//! Doubles use Java's `Double.toString` (`3.0`, `1.0E20`, `1.0E-4`). Object key order is kept as
//! given, so callers must build values with fields in proto field order.

use serde_json::Value;

/// Pretty-prints `value` the way protobuf-java's `JsonFormat` does (no trailing newline).
pub fn to_java_pretty(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, 0);
    out
}

fn write_value(out: &mut String, value: &Value, level: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => match n.as_f64().filter(|_| n.is_f64()) {
            Some(f) => out.push_str(&java_double(f)),
            None => out.push_str(&n.to_string()),
        },
        Value::String(s) => write_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(out, item, level);
            }
            out.push(']');
        }
        Value::Object(fields) => {
            out.push_str("{\n");
            for (i, (key, v)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                indent(out, level + 1);
                write_string(out, key);
                out.push_str(": ");
                write_value(out, v, level + 1);
            }
            if !fields.is_empty() {
                out.push('\n');
            }
            indent(out, level);
            out.push('}');
        }
    }
}

fn indent(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("  ");
    }
}

/// Escapes what JSON requires plus U+2028/U+2029 (as Gson does); other non-ASCII text is
/// emitted as-is.
fn write_string(out: &mut String, s: &str) {
    write_escaped(out, s, false);
}

/// Error bodies use a compact form that additionally escapes `/` as `\/`:
/// `{"error":{"code":400,"message":"...","status":"INVALID_ARGUMENT"}}`. An empty message is
/// omitted.
pub fn error_body(code: u16, message: &str, status: &str) -> String {
    let mut out = format!("{{\"error\":{{\"code\":{code},");
    if !message.is_empty() {
        out.push_str("\"message\":");
        write_escaped(&mut out, message, true);
        out.push(',');
    }
    out.push_str("\"status\":");
    write_escaped(&mut out, status, true);
    out.push_str("}}");
    out
}

fn write_escaped(out: &mut String, s: &str, escape_slash: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '/' if escape_slash => out.push_str("\\/"),
            c if c < ' ' || c == '\u{2028}' || c == '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Java's `Double.toString`: the shortest round-tripping digits, in plain notation for
/// magnitudes in [1e-3, 1e7) and `d.dddE±n` otherwise, always with a fractional digit.
pub fn java_double(d: f64) -> String {
    if d.is_nan() {
        return "NaN".into();
    }
    if d.is_infinite() {
        return if d > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    let sign = if d.is_sign_negative() { "-" } else { "" };
    if d == 0.0 {
        return format!("{sign}0.0");
    }
    // `{:e}` yields the shortest round-trip digits, e.g. "1.23456789e7".
    let sci = format!("{:e}", d.abs());
    let (mantissa, exp) = sci.split_once('e').expect("scientific format");
    let exp: i32 = exp.parse().expect("exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();

    let abs = d.abs();
    if (1e-3..1e7).contains(&abs) {
        let point = exp + 1; // digits before the decimal point
        let (int_part, frac_part) = if point <= 0 {
            (
                "0".to_string(),
                format!("{}{digits}", "0".repeat((-point) as usize)),
            )
        } else if point as usize >= digits.len() {
            (
                format!("{digits}{}", "0".repeat(point as usize - digits.len())),
                String::new(),
            )
        } else {
            (
                digits[..point as usize].to_string(),
                digits[point as usize..].to_string(),
            )
        };
        let frac = if frac_part.is_empty() {
            "0"
        } else {
            &frac_part
        };
        format!("{sign}{int_part}.{frac}")
    } else {
        let frac = if digits.len() > 1 { &digits[1..] } else { "0" };
        format!("{sign}{}.{frac}E{exp}", &digits[..1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn doubles_match_java() {
        for (d, want) in [
            (1.5, "1.5"),
            (3.0, "3.0"),
            (-2.0, "-2.0"),
            (1e20, "1.0E20"),
            (1e-4, "1.0E-4"),
            (12345678.9, "1.23456789E7"),
            (1234567.0, "1234567.0"),
            (0.001, "0.001"),
            (0.1, "0.1"),
            (100.0, "100.0"),
            (1e7, "1.0E7"),
            (-0.0, "-0.0"),
            (f64::NAN, "NaN"),
            (f64::NEG_INFINITY, "-Infinity"),
            (f64::MAX, "1.7976931348623157E308"),
            (f64::MIN_POSITIVE, "2.2250738585072014E-308"),
        ] {
            assert_eq!(java_double(d), want, "{d:e}");
        }
    }

    #[test]
    fn layout_matches_jsonformat() {
        let v = json!({
            "mutationResults": [{"key": {"path": [{"kind": "K", "id": "1"}]}, "version": "2"}, {"version": "3"}],
            "empty": {},
            "nums": [1, 2],
            "d": 3.0,
            "s": "é\u{1}\"\n<>&=/"
        });
        let want = r#"{
  "mutationResults": [{
    "key": {
      "path": [{
        "kind": "K",
        "id": "1"
      }]
    },
    "version": "2"
  }, {
    "version": "3"
  }],
  "empty": {
  },
  "nums": [1, 2],
  "d": 3.0,
  "s": "é\u0001\"\n<>&=/"
}"#;
        assert_eq!(to_java_pretty(&v), want);
    }

    #[test]
    fn error_bodies_match_java() {
        assert_eq!(
            error_body(
                400,
                "Illegal string \"é<>&'=\\/\u{2028}\n\" in namespace id.",
                "INVALID_ARGUMENT"
            ),
            r#"{"error":{"code":400,"message":"Illegal string \"é<>&'=\\\/\u2028\n\" in namespace id.","status":"INVALID_ARGUMENT"}}"#
        );
        assert_eq!(
            error_body(500, "", "UNKNOWN"),
            r#"{"error":{"code":500,"status":"UNKNOWN"}}"#
        );
    }

    #[test]
    fn empty_message() {
        assert_eq!(to_java_pretty(&json!({})), "{\n}");
    }
}
