use crate::command::style as tty_style;

pub fn truncate_end(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut out = String::new();
    for c in value.chars().take(max.saturating_sub(1)) {
        out.push(c);
    }
    out.push('…');
    out
}

pub fn pluralize<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

pub fn join_dim_bullets(parts: &[String]) -> String {
    if parts.is_empty() {
        String::new()
    } else {
        parts.join(&format!(" {} ", tty_style::dim("·")))
    }
}
