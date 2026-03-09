/// Simple URL encoding for query parameters.
pub fn url_encode(s: &str) -> String {
    let mut result = String::with_capacity(s.len() * 3);
    for ch in s.bytes() {
        match ch {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(ch as char);
            }
            b' ' => result.push('+'),
            _ => {
                result.push('%');
                result.push_str(&format!("{:02X}", ch));
            }
        }
    }
    result
}

/// Strip trailing parenthetical suffixes like "(Deluxe Edition)", "(Dolby Atmos)", etc.
pub fn clean_title(title: &str) -> &str {
    match title.rfind('(') {
        Some(pos) if pos > 0 => title[..pos].trim_end(),
        _ => title,
    }
}

/// Convert a string into a URL-friendly slug.
/// "good kid, m.A.A.d city" -> "good-kid-maad-city"
pub fn slugify(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            result.push(ch.to_ascii_lowercase());
        } else if ch == ' ' || ch == '-' || ch == ':' {
            result.push('-');
        }
    }
    // Collapse consecutive hyphens
    let mut collapsed = String::with_capacity(result.len());
    let mut prev_hyphen = false;
    for ch in result.chars() {
        if ch == '-' {
            if !prev_hyphen {
                collapsed.push('-');
            }
            prev_hyphen = true;
        } else {
            collapsed.push(ch);
            prev_hyphen = false;
        }
    }
    collapsed.trim_matches('-').to_string()
}
