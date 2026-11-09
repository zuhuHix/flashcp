//! The helpers' own output: one JSON line in the same shape flashcp uses, so the GUI needs a
//! single parser.

use std::io::Write;

use crate::identity::Identity;

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

/// `{"event":"error","message":"<prog>: <reason>"}`.
pub fn error_line(prog: &str, reason: &str) -> String {
    format!("{{\"event\":\"error\",\"message\":{}}}", quote(&format!("{prog}: {reason}")))
}

/// `{"event":"trusted","key":..,"model":..,"size":N}`.
pub fn trusted_line(id: &Identity) -> String {
    format!("{{\"event\":\"trusted\",\"key\":{},\"model\":{},\"size\":{}}}", quote(&id.key), quote(&id.model), id.size)
}

pub fn print_line(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes() {
        assert_eq!(
            error_line("flashcp-helper", "bad \"x\"\\\n\u{1}é"),
            r#"{"event":"error","message":"flashcp-helper: bad \"x\"\\\n\u0001é"}"#
        );
        let id = Identity { key: "A\"1".into(), wwn: "-".into(), model: "Ultra".into(), size: 5 };
        let v: serde_json::Value = serde_json::from_str(&trusted_line(&id)).unwrap();
        assert_eq!(v, serde_json::json!({"event":"trusted","key":"A\"1","model":"Ultra","size":5}));
    }
}
