/// Last integer in `text`, clamped to the vote range. None if no digits.
pub fn parse_score(text: &str) -> Option<i32> {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut last = None;
    while i < bytes.len() {
        let neg = bytes[i] == b'-';
        let start = if neg { i + 1 } else { i };
        if start < bytes.len() && bytes[start].is_ascii_digit() {
            let mut j = start;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            let s = &text[i..j];
            let n = s
                .parse::<i64>()
                .ok()
                .map(|n| n.clamp(-50, 50) as i32);
            if n.is_some() {
                last = n;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    last
}

pub fn has_substr(hay: &str, needle: &str) -> bool {
    !needle.is_empty() && hay.contains(needle)
}

pub fn before(text: &str, needle: &str) -> String {
    if needle.is_empty() {
        return text.to_string();
    }
    match text.find(needle) {
        Some(i) => text[..i].trim().to_string(),
        None => text.to_string(),
    }
}

pub fn cut(text: &str, needle: &str) -> String {
    if needle.is_empty() {
        return text.to_string();
    }
    text.replace(needle, "").trim().to_string()
}

pub fn strip_tags(text: &str) -> String {
    let mut out = text.to_string();
    for tag in ["thought", "response", "message", "declaration"] {
        out = out.replace(&format!("<{tag}>"), "");
        out = out.replace(&format!("</{tag}>"), "");
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_number_wins() {
        assert_eq!(parse_score("start 50 then I prefer B: -12"), Some(-12));
        assert_eq!(parse_score("no score here"), None);
        assert_eq!(parse_score("999"), Some(50));
        assert_eq!(parse_score("-999"), Some(-50));
    }

    #[test]
    fn declaration_marker() {
        assert!(has_substr("ok !declaration stay", "!declaration"));
        assert!(!has_substr("hello", "!declaration"));
    }

    #[test]
    fn before_cuts_at_needle() {
        assert_eq!(before("hello [Response] hello", "[Response]"), "hello");
        assert_eq!(before("just once", "[Response]"), "just once");
    }

    #[test]
    fn strips_memory_tags() {
        assert_eq!(strip_tags("<thought>hi</thought>"), "hi");
        assert_eq!(strip_tags("  <response>yo</response>  "), "yo");
    }
}
