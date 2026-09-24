//! Small text helpers: HTML → plain text, relative dates, avatar letters/colours.

use chrono::{DateTime, Datelike, Local};

/// Strips tags, decodes entities and collapses whitespace.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len().min(4096));
    let mut in_tag = false;
    let mut skip_until: Option<&str> = None;
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    let bytes = html.as_bytes();
    while i < bytes.len() {
        if let Some(end) = skip_until {
            match lower[i..].find(end) {
                Some(p) => {
                    i += p + end.len();
                    skip_until = None;
                    continue;
                }
                None => break,
            }
        }
        let c = bytes[i];
        if in_tag {
            if c == b'>' {
                in_tag = false;
                out.push(' ');
            }
            i += 1;
            continue;
        }
        if c == b'<' {
            let rest = &lower[i..];
            if rest.starts_with("<script") {
                skip_until = Some("</script>");
            } else if rest.starts_with("<style") {
                skip_until = Some("</style>");
            } else if rest.starts_with("<!--") {
                skip_until = Some("-->");
            } else {
                in_tag = true;
            }
            i += 1;
            continue;
        }
        // Copy the full UTF-8 sequence starting at `i`.
        let ch_len = utf8_len(c);
        let end = (i + ch_len).min(bytes.len());
        out.push_str(&html[i..end]);
        i = end;
    }
    let decoded = html_escape::decode_html_entities(&out);
    collapse_ws(&decoded)
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

pub fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for word in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// Truncates to `max` characters on a word boundary, adding an ellipsis.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    let cut = match cut.rfind(' ') {
        Some(p) if p > max / 2 => &cut[..p],
        _ => &cut[..],
    };
    format!("{}…", cut.trim_end_matches([',', '.', ';', ':', ' ']))
}

/// Human friendly relative time: "just now", "5m", "3h", "2d", "Sep 3", "Sep 3, 2024".
pub fn ago(ts: i64, now: i64) -> String {
    let d = (now - ts).max(0);
    match d {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86_399 => format!("{}h ago", d / 3600),
        86_400..=604_799 => format!("{}d ago", d / 86_400),
        _ => {
            let Some(t) = DateTime::from_timestamp(ts, 0) else {
                return String::new();
            };
            let t = t.with_timezone(&Local);
            if t.year() == Local::now().year() {
                t.format("%b %-d").to_string()
            } else {
                t.format("%b %-d, %Y").to_string()
            }
        }
    }
}

/// First letter/digit of a title, uppercased, for the avatar badge.
pub fn initial(title: &str) -> String {
    title
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "•".into())
}

/// Stable 64-bit FNV-1a hash (used for cache file names and avatar colours).
pub fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

// Muted, ink-like editorial colours: distinct per source, never neon.
const TINTS: [(u8, u8, u8); 10] = [
    (0x1d, 0x4e, 0x89), // newsroom blue
    (0xa8, 0x6a, 0x0b), // ochre
    (0x2e, 0x6b, 0x45), // forest
    (0x7a, 0x3b, 0x69), // plum
    (0x0f, 0x6e, 0x73), // teal
    (0xb3, 0x54, 0x1e), // rust
    (0x4b, 0x5a, 0x8a), // slate
    (0x6b, 0x6b, 0x1f), // olive
    (0x9c, 0x2a, 0x4f), // claret
    (0x5c, 0x4a, 0x3a), // walnut
];

pub fn tint(seed: &str) -> slint::Color {
    let (r, g, b) = TINTS[(fnv1a(seed) % TINTS.len() as u64) as usize];
    slint::Color::from_rgb_u8(r, g, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_html() {
        let s = html_to_text("<p>Hello&nbsp;<b>world</b> &amp; <script>x()</script>friends</p>");
        assert_eq!(s, "Hello world & friends");
    }

    #[test]
    fn keeps_unicode() {
        assert_eq!(html_to_text("<i>caffè</i> — ok"), "caffè — ok");
    }

    #[test]
    fn truncates_on_word() {
        assert_eq!(truncate("one two three four", 9), "one two…");
        assert_eq!(truncate("short", 10), "short");
    }

    #[test]
    fn relative_times() {
        assert_eq!(ago(1000, 1030), "just now");
        assert_eq!(ago(0, 180), "3m ago");
        assert_eq!(ago(0, 7200), "2h ago");
        assert_eq!(ago(0, 86_400 * 3), "3d ago");
    }
}
