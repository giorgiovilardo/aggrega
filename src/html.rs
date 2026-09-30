//! A forgiving tag-soup HTML tokenizer and the small helpers built on it.
//! Every piece of HTML the app looks at (feed titles and summaries, article
//! bodies, whole web pages) goes through `tokenize`.

use crate::text;

#[derive(Debug, PartialEq)]
pub enum Token<'a> {
    /// An opening tag: its lowercased name and the raw tag text (`<a href=…>`),
    /// to read attributes from with `attr`.
    Open {
        name: String,
        tag: &'a str,
    },
    Close(String),
    /// Raw text between tags, entities not yet decoded.
    Text(&'a str),
}

/// Elements whose contents are never text. (`noscript` isn't one: it holds
/// markup, often the real `<img>` behind a lazy-loading placeholder.)
const RAW: &[&str] = &["script", "style", "svg", "template", "math", "textarea"];
/// Elements that never have a closing tag.
const VOID: &[&str] = &[
    "img", "br", "hr", "input", "meta", "link", "source", "wbr", "area", "col", "embed", "param",
    "track", "base",
];

/// Splits HTML into tags and text. Never fails: good enough for pulling text
/// out of real-world pages. Comments, doctypes and the contents of `RAW`
/// elements are dropped, and a `<` that can't start a tag (`1 < 2`) is text.
pub fn tokenize(html: &str) -> Vec<Token<'_>> {
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut text_start = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &lower[i..];
        let next = rest.as_bytes().get(1).copied().unwrap_or(b' ');
        let is_tag = next.is_ascii_alphabetic() || next == b'/' || next == b'!' || next == b'?';
        if !is_tag {
            i += 1;
            continue;
        }
        if text_start < i {
            out.push(Token::Text(&html[text_start..i]));
        }
        if rest.starts_with("<!--") {
            i = rest.find("-->").map_or(bytes.len(), |p| i + p + 3);
            text_start = i;
            continue;
        }
        let end = rest.find('>').map_or(bytes.len(), |p| i + p + 1);
        let tag = &html[i..end];
        let closing = next == b'/';
        let name: String = lower[i + 1 + closing as usize..end]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        i = end;
        text_start = end;
        if name.is_empty() {
            continue; // <!doctype>, <?xml?>
        }
        if closing {
            out.push(Token::Close(name));
            continue;
        }
        if RAW.contains(&name.as_str()) {
            // Skip to the matching close tag.
            let close = format!("</{name}");
            i = lower[i..].find(&close).map_or(bytes.len(), |p| {
                let at = i + p;
                lower[at..].find('>').map_or(bytes.len(), |q| at + q + 1)
            });
            text_start = i;
            continue;
        }
        out.push(Token::Open { name, tag });
    }
    if text_start < bytes.len() {
        out.push(Token::Text(&html[text_start..]));
    }
    out
}

/// Index just past the element opened at `tokens[start]` (or the end).
pub fn element_end(tokens: &[Token], start: usize) -> usize {
    let Token::Open { name, .. } = &tokens[start] else {
        return start + 1;
    };
    if VOID.contains(&name.as_str()) {
        return start + 1;
    }
    let mut depth = 0usize;
    for (i, t) in tokens.iter().enumerate().skip(start) {
        match t {
            Token::Open { name: n, .. } if n == name => depth += 1,
            Token::Close(n) if n == name => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
    }
    tokens.len()
}

/// The raw text of every `<name …>` opening tag in `html`, in order.
pub fn open_tags<'a>(html: &'a str, name: &'a str) -> impl Iterator<Item = &'a str> {
    tokenize(html).into_iter().filter_map(move |t| match t {
        Token::Open { name: n, tag } if n == name => Some(tag),
        _ => None,
    })
}

/// Reads an attribute value from a single HTML tag (quoted or unquoted).
/// Entities in the value are left as they are.
pub fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(p) = lower[from..].find(name) {
        let p = from + p;
        from = p + name.len();
        let before_ok = p > 0 && lower.as_bytes()[p - 1].is_ascii_whitespace();
        let rest = lower[from..].trim_start();
        if !before_ok || !rest.starts_with('=') {
            continue;
        }
        let offset = tag.len() - rest.len() + 1;
        let value = tag[offset..].trim_start();
        return Some(match value.chars().next()? {
            q @ ('"' | '\'') => value[1..].split(q).next()?.to_string(),
            _ => value
                .split(|c: char| c.is_whitespace() || c == '>')
                .next()?
                .to_string(),
        });
    }
    None
}

/// Plain text: tags become word breaks, entities are decoded and whitespace
/// is collapsed.
pub fn to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len().min(4096));
    for t in tokenize(html) {
        match t {
            Token::Text(s) => out.push_str(s),
            Token::Open { .. } | Token::Close(_) => out.push(' '),
        }
    }
    text::collapse_ws(&html_escape::decode_html_entities(&out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open<'a>(name: &str, tag: &'a str) -> Token<'a> {
        Token::Open {
            name: name.into(),
            tag,
        }
    }

    fn close(name: &str) -> Token<'static> {
        Token::Close(name.into())
    }

    #[test]
    fn tokenizes_tags_and_text() {
        assert_eq!(
            tokenize(r#"<P Class="X">Hi <B>there</B></p>"#),
            vec![
                // Names are lowercased; the tag keeps its spelling for `attr`.
                open("p", r#"<P Class="X">"#),
                Token::Text("Hi "),
                open("b", "<B>"),
                Token::Text("there"),
                close("b"),
                close("p"),
            ]
        );
    }

    #[test]
    fn a_lone_less_than_is_text() {
        assert_eq!(
            tokenize("1 < 2 and x <3"),
            vec![Token::Text("1 < 2 and x <3")]
        );
        assert_eq!(tokenize("<"), vec![Token::Text("<")]);
        // A letter after `<` does start a tag, even without a `>`.
        assert_eq!(
            tokenize("a <b never closed"),
            vec![Token::Text("a "), open("b", "<b never closed")]
        );
    }

    #[test]
    fn drops_comments_doctypes_and_raw_elements() {
        let html = r#"<!DOCTYPE html><?xml version="1.0"?>A<!-- <p>no</p> -->B
            <script>document.write("<p>no</p>")</script><SVG><text>no</text></svg>C"#;
        let text: String = tokenize(html)
            .iter()
            .map(|t| match t {
                Token::Text(s) => s.trim(),
                _ => panic!("only text should survive: {t:?}"),
            })
            .collect();
        assert_eq!(text, "ABC");
        // <noscript> holds markup, so its tags come through.
        assert_eq!(
            tokenize("<noscript><img src=a></noscript>"),
            vec![
                open("noscript", "<noscript>"),
                open("img", "<img src=a>"),
                close("noscript")
            ]
        );
        // Unterminated comments and raw elements swallow the rest.
        assert_eq!(tokenize("A<!-- open"), vec![Token::Text("A")]);
        assert_eq!(tokenize("A<style>p{}"), vec![Token::Text("A")]);
    }

    #[test]
    fn finds_where_elements_end() {
        let t = tokenize("<div><div>a</div>b</div><p>c</p>");
        // The outer div spans its nested namesake.
        assert_eq!(element_end(&t, 0), 6);
        assert_eq!(element_end(&t, 1), 4);
        // Void elements end at themselves, unclosed ones at the end of input.
        let t = tokenize("<p><img src=a>text<section>open");
        assert_eq!(element_end(&t, 1), 2);
        assert_eq!(element_end(&t, 3), t.len());
        // Anything but an opening tag is its own element.
        assert_eq!(element_end(&t, 2), 3);
    }

    #[test]
    fn reads_attributes() {
        let t = r#"<link REL="alternate" type='application/rss+xml' href=/Feed.xml>"#;
        assert_eq!(attr(t, "rel").as_deref(), Some("alternate"));
        assert_eq!(attr(t, "type").as_deref(), Some("application/rss+xml"));
        // Unquoted values stop at `>`, and keep their case.
        assert_eq!(attr(t, "href").as_deref(), Some("/Feed.xml"));
        assert_eq!(attr(t, "title"), None);
        // Only whole attribute names count: `src` isn't read from `data-src`.
        let img = r#"<img data-src="lazy.jpg" alt="src=x">"#;
        assert_eq!(attr(img, "src"), None);
        assert_eq!(attr(img, "data-src").as_deref(), Some("lazy.jpg"));
    }

    #[test]
    fn lists_opening_tags_by_name() {
        let html = r#"<img src="a.png"><p>x</p><!-- <img src="hidden.png"> -->
            <imgx src="no.png"><IMG SRC="b.png"></img>"#;
        assert_eq!(
            open_tags(html, "img").collect::<Vec<_>>(),
            vec![r#"<img src="a.png">"#, r#"<IMG SRC="b.png">"#]
        );
    }

    #[test]
    fn converts_to_plain_text() {
        assert_eq!(
            to_text("<p>Hello&nbsp;<b>world</b> &amp; <script>x()</script>friends</p>"),
            "Hello world & friends"
        );
        assert_eq!(to_text("<i>caffè</i> — ok"), "caffè — ok");
        // Tags separate words, so paragraphs don't run together.
        assert_eq!(to_text("<p>One</p><p>Two</p>"), "One Two");
        // Escaped markup is text, not a tag to strip.
        assert_eq!(to_text("use &lt;b&gt; for bold"), "use <b> for bold");
    }

    #[test]
    fn keeps_text_after_a_lone_less_than() {
        // Feed titles arrive entity-decoded by the XML parser, so a title
        // written `1 &lt; 2` reaches us as `1 < 2`.
        assert_eq!(to_text("1 < 2 matters"), "1 < 2 matters");
        assert_eq!(to_text("I <3 Rust"), "I <3 Rust");
    }
}
