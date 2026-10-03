/// Remote text must never become terminal commands. Keep each field on one
/// line so embedded newlines cannot also corrupt menus or playlist metadata.
pub fn text(value: &str) -> String {
    console::strip_ansi_codes(value)
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// JSON already escapes ASCII controls. Escape C1 controls too, retaining the
/// original data when parsed while making JSON safe to print in a terminal.
pub fn json(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for c in value.chars() {
        if ('\u{7f}'..='\u{9f}').contains(&c) {
            use std::fmt::Write;
            write!(&mut output, "\\u{:04x}", c as u32).expect("writing to a String cannot fail");
        } else {
            output.push(c);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_screen_clipboard_and_link_commands() {
        for value in [
            "Anime\x1b[2J",
            "Anime\x1b]52;c;dGVzdA==\x07",
            "Anime\x1b]52;c;dGVzdA==\x1b\\",
            "\x1b]8;;https://bad.test\x1b\\Anime\x1b]8;;\x1b\\",
        ] {
            assert_eq!(text(value), "Anime");
        }
        assert_eq!(text("日本語\nTitle\r\u{9b}31m"), "日本語 Title ");
    }

    #[test]
    fn json_control_escaping_preserves_data() {
        let value = serde_json::json!({"name": "Anime\u{9b}2J\x1b]52;c;data\x07"});
        let output = json(&serde_json::to_string_pretty(&value).unwrap());
        assert!(!output.contains('\u{9b}'));
        assert!(!output.contains('\x1b'));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&output).unwrap(),
            value
        );
    }
}
