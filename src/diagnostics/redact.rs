use super::MESSAGE_BYTES;

// A second line of defence for errors and older journal entries. Producers must
// still avoid logging credentials, window titles or user-authored configuration.
pub(super) fn sanitize(message: &str, home: &str) -> String {
    let message = if home.len() > 1 {
        message.replace(home, "<home>")
    } else {
        message.to_owned()
    };
    let mut result = String::new();
    for word in message.split_whitespace() {
        if !result.is_empty() {
            result.push(' ');
        }
        let lower = word.to_ascii_lowercase();
        let sensitive = [
            "password",
            "passwd",
            "token",
            "secret",
            "authorization",
            "serial",
            "username",
        ]
        .iter()
        .any(|key| lower.trim_start_matches(['"', '\'', '{']).starts_with(key));
        if sensitive || lower.starts_with("proxy-authorization") {
            // Values may be quoted, contain whitespace, or use a Bearer scheme.
            // Do not try to guess where an arbitrary diagnostic's secret ends.
            result.push_str("<redacted>");
            break;
        } else if lower.contains("://") {
            result.push_str("<url>");
        } else if lower.contains("/home/") || lower.contains("/root/") {
            result.push_str("<home-path>");
        } else if lower.contains("046d:") || looks_like_identifier(&lower) {
            result.push_str("<device>");
        } else {
            result.extend(word.chars().filter(|c| !c.is_control()));
        }
        if result.len() >= MESSAGE_BYTES {
            break;
        }
    }
    if result.len() > MESSAGE_BYTES {
        result.truncate(result.floor_char_boundary(MESSAGE_BYTES - '…'.len_utf8()));
        result.push('…');
    }
    result
}

fn looks_like_identifier(word: &str) -> bool {
    let word = word.trim_matches(|c: char| !c.is_ascii_hexdigit());
    (word.len() >= 8 && word.bytes().all(|byte| byte.is_ascii_hexdigit()))
        || (word.len() == 17
            && word.split(':').count() == 6
            && word
                .split(':')
                .all(|part| part.len() == 2 && part.bytes().all(|byte| byte.is_ascii_hexdigit())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_urls_homes_credentials_and_identifiers() {
        let text = sanitize(
            "failed /custom/person/config https://user:password@proxy.example:8080?token=x token=abc password: xyz 046d:c548:ABCDEF12:1 AA:BB:CC:DD:EE:FF DEADBEEF",
            "/custom/person",
        );
        for private in [
            "person",
            "user:",
            "password@",
            "proxy.example",
            "abc",
            "xyz",
            "ABCDEF12",
            "AA:BB",
            "DEADBEEF",
        ] {
            assert!(!text.contains(private), "{text}");
        }
        assert!(text.contains("<home>/config"));
    }

    #[test]
    fn sanitization_is_bounded_unicode_safe_and_idempotent() {
        let text = sanitize(&format!("{}\u{1b}[31m", "测试".repeat(500)), "");
        assert!(text.len() <= MESSAGE_BYTES);
        assert!(!text.contains('\u{1b}'));
        assert_eq!(sanitize(&text, ""), text);
        let boundary = format!("{} password: hidden", "a".repeat(MESSAGE_BYTES - 1));
        assert!(sanitize(&boundary, "").len() <= MESSAGE_BYTES);
    }

    #[test]
    fn multiline_and_quoted_credentials_are_not_partially_exposed() {
        for message in [
            "Network error: password = \"two secret words\"",
            "Network error: {\"password\":\"two secret words\"}",
            "Network error: Authorization: Bearer abc123\nprivate suffix",
            "Network error: Proxy-Authorization: Basic abc123",
        ] {
            let safe = sanitize(message, "");
            assert_eq!(safe, "Network error: <redacted>");
            assert_eq!(sanitize(&safe, ""), safe);
        }
    }
}
