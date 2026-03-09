/// Extract the first JSON-LD block from HTML that contains a Review.
pub fn extract_json_ld(html: &str) -> Option<String> {
    let marker = "application/ld+json";
    let mut search_from = 0;

    loop {
        let tag_pos = html[search_from..].find(marker)?;
        let abs_pos = search_from + tag_pos;

        let content_start = html[abs_pos..].find('>')? + abs_pos + 1;
        let content_end = html[content_start..].find("</script>")? + content_start;
        let json_str = html[content_start..content_end].trim();

        if json_str.contains("\"Review\"") || json_str.contains("\"reviewBody\"") {
            if json_str.starts_with('[') {
                if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(json_str) {
                    for item in &arr {
                        let s = item.to_string();
                        if s.contains("\"Review\"") || s.contains("\"reviewBody\"") {
                            return Some(s);
                        }
                    }
                }
            }
            return Some(json_str.to_string());
        }

        search_from = content_end;
        if search_from >= html.len().saturating_sub(50) {
            break;
        }
    }

    None
}

/// Extract the content of a `<script>` tag containing the given marker string.
#[allow(dead_code)]
pub fn extract_script_content<'a>(html: &'a str, marker: &str) -> Option<&'a str> {
    let script_tag = "<script";
    let mut search_from = 0;

    loop {
        let tag_pos = html[search_from..].find(script_tag)?;
        let abs_pos = search_from + tag_pos;

        let content_start = html[abs_pos..].find('>')? + abs_pos + 1;
        let content_end = html[content_start..].find("</script>")? + content_start;
        let content = &html[content_start..content_end];

        if content.contains(marker) {
            return Some(content);
        }

        search_from = content_end;
        if search_from >= html.len().saturating_sub(50) {
            break;
        }
    }

    None
}

/// Strip HTML tags from a string, keeping only text content.
pub fn strip_html_tags(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }
    result
}
