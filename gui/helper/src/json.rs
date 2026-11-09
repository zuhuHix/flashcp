//! The helper's own error report: one JSON line in the same shape flashcp uses, so the GUI needs
//! a single parser.

use std::io::Write;

/// JSON string literal (with quotes) for `s`.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn error_line(reason: &str) -> String {
    format!("{{\"event\":\"error\",\"message\":{}}}", quote(&format!("flashcp-helper: {reason}")))
}

pub fn print_error(reason: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", error_line(reason));
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes() {
        assert_eq!(
            error_line("bad \"x\"\\\n\u{1}é"),
            r#"{"event":"error","message":"flashcp-helper: bad \"x\"\\\n\u0001é"}"#
        );
    }
}
