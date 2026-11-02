//! Link timestamps without interpreting user comments as Markdown or HTML.
fn seconds(token: &str) -> Option<u32> {
    let parts: Vec<_> = token.split(':').collect();
    if !(2..=3).contains(&parts.len()) || parts[0].is_empty() {
        return None;
    }
    let mut total = parts[0].parse::<u32>().ok()?;
    for part in &parts[1..] {
        let value = part.parse::<u32>().ok()?;
        if part.len() != 2 || value >= 60 {
            return None;
        }
        total = total.checked_mul(60)?.checked_add(value)?;
    }
    Some(total)
}

fn markdown(text: &str) -> String {
    let mut result = String::new();
    let mut chars = text.char_indices().peekable();
    let mut previous = None;
    while let Some((start, ch)) = chars.next() {
        if ch.is_ascii_digit() {
            let mut end = start + 1;
            while let Some(&(index, next)) = chars.peek() {
                if !next.is_ascii_digit() && next != ':' {
                    break;
                }
                end = index + next.len_utf8();
                chars.next();
            }
            let token = &text[start..end];
            let boundary = |c: char| c.is_alphanumeric() || matches!(c, '_' | ':' | '/' | '.');
            if !previous.is_some_and(boundary)
                && !chars
                    .peek()
                    .is_some_and(|(_, c)| c.is_alphanumeric() || *c == '_')
                && let Some(time) = seconds(token)
            {
                result.push_str(&format!("[{token}](comment-time:{time})"));
            } else {
                result.push_str(token);
            }
            previous = token.chars().last();
        } else {
            if ch == '\n' {
                result.push_str("  \n");
            } else {
                if ch.is_ascii_punctuation() {
                    result.push('\\');
                }
                result.push(ch);
            }
            previous = Some(ch);
        }
    }
    result
}

pub fn format(text: &str) -> slint::StyledText {
    slint::StyledText::from_markdown(&markdown(text))
        .unwrap_or_else(|_| slint::StyledText::from_plain_text(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_multiple_timestamps_and_hours() {
        assert_eq!(
            markdown("11:44 wow 1:02:03!"),
            "[11:44](comment-time:704) wow [1:02:03](comment-time:3723)\\!"
        );
        assert!(markdown("(0:00)\n123:45").contains("[123:45](comment-time:7425)"));
    }

    #[test]
    fn rejects_invalid_and_embedded_times() {
        for text in [
            "1:60",
            "1:2",
            "1:02:99",
            "1:02:03:04",
            "abc1:23",
            "1:23abc",
            "https://example.com/1:23",
            "999999999999:00",
        ] {
            assert!(!markdown(text).contains("comment-time:"), "{text}");
        }
    }

    #[test]
    fn escapes_comment_markup() {
        assert_eq!(
            markdown("<b>*hi*</b> [x](evil)\n0:05"),
            "\\<b\\>\\*hi\\*\\<\\/b\\> \\[x\\]\\(evil\\)  \n[0:05](comment-time:5)"
        );
    }
}
