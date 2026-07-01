//! Parse a COACH-prompt Markdown summary into its `## ` sections so the embed
//! layer can map them onto Discord fields.

/// An ordered list of `(heading, body)` sections parsed from the coach Markdown.
pub struct Summary {
    pub sections: Vec<(String, String)>,
}

/// Split Markdown on `## ` headings. Any text before the first heading is
/// ignored; section order is preserved.
pub fn parse(md: &str) -> Summary {
    let mut sections = Vec::new();
    let mut heading: Option<String> = None;
    let mut body = String::new();

    for line in md.lines() {
        if let Some(h) = line.strip_prefix("## ") {
            if let Some(prev) = heading.take() {
                sections.push((prev, body.trim().to_string()));
            }
            heading = Some(h.trim().to_string());
            body.clear();
        } else if heading.is_some() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some(prev) = heading.take() {
        sections.push((prev, body.trim().to_string()));
    }
    Summary { sections }
}
