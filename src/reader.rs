//! Reader view content: turns article HTML (from the feed, or the full web
//! page) into a flat list of simple blocks the UI can lay out, and stores
//! those blocks compactly in the database.

use url::Url;

use crate::fetch::attr;
use crate::text;

/// Below this many characters of text, the feed only carried a summary and
/// the reader fetches the full page.
const FULL_TEXT_CHARS: usize = 1200;
/// A page region needs at least this much text to count as the article.
const MIN_REGION_CHARS: usize = 400;
/// In the whole-page fallback, shorter paragraphs are treated as boilerplate.
const MIN_FALLBACK_PARAGRAPH: usize = 80;
const MAX_BLOCKS: usize = 1500;
const MAX_IMAGES: usize = 24;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Heading(String),
    Paragraph(String),
    Quote(String),
    Bullet(String),
    Code(String),
    Image(String),
}

impl Block {
    fn text_len(&self) -> usize {
        match self {
            Block::Image(_) => 0,
            Block::Heading(t)
            | Block::Paragraph(t)
            | Block::Quote(t)
            | Block::Bullet(t)
            | Block::Code(t) => t.chars().count(),
        }
    }
}

/// Characters of readable text in `blocks`.
pub fn text_len(blocks: &[Block]) -> usize {
    blocks.iter().map(Block::text_len).sum()
}

/// Whether the feed's own content is too short to read and the full page
/// should be fetched.
pub fn needs_full_page(blocks: &[Block]) -> bool {
    text_len(blocks) < FULL_TEXT_CHARS
}

/// Pages where scraping the HTML can't produce an article (video sites).
pub fn is_scrapable(link: &str) -> bool {
    let Ok(u) = Url::parse(link) else {
        return false;
    };
    let host = u.host_str().unwrap_or("");
    matches!(u.scheme(), "http" | "https")
        && !["youtube.com", "youtu.be", "vimeo.com"]
            .iter()
            .any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

// ---- storage format ------------------------------------------------------
//
// One block per line: a one-letter kind, a space, then the text with `\` and
// newlines escaped. Compact, and trivially forward compatible.

pub fn encode(blocks: &[Block]) -> String {
    let mut out = String::new();
    for b in blocks {
        let (kind, t) = match b {
            Block::Heading(t) => ('h', t),
            Block::Paragraph(t) => ('p', t),
            Block::Quote(t) => ('q', t),
            Block::Bullet(t) => ('l', t),
            Block::Code(t) => ('c', t),
            Block::Image(t) => ('i', t),
        };
        out.push(kind);
        out.push(' ');
        for ch in t.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                _ => out.push(ch),
            }
        }
        out.push('\n');
    }
    out
}

pub fn decode(s: &str) -> Vec<Block> {
    s.lines()
        .filter_map(|line| {
            let (kind, raw) = line.split_once(' ')?;
            let mut t = String::with_capacity(raw.len());
            let mut chars = raw.chars();
            while let Some(ch) = chars.next() {
                if ch == '\\' {
                    match chars.next() {
                        Some('n') => t.push('\n'),
                        Some(other) => t.push(other),
                        None => {}
                    }
                } else {
                    t.push(ch);
                }
            }
            Some(match kind {
                "h" => Block::Heading(t),
                "p" => Block::Paragraph(t),
                "q" => Block::Quote(t),
                "l" => Block::Bullet(t),
                "c" => Block::Code(t),
                "i" => Block::Image(t),
                _ => return None,
            })
        })
        .collect()
}

// ---- HTML tokenizer ------------------------------------------------------

#[derive(Debug)]
enum Token<'a> {
    Open { name: String, tag: &'a str },
    Close(String),
    Text(&'a str),
}

/// Elements whose contents are never text.
const RAW: [&str; 7] = [
    "script", "style", "noscript", "svg", "template", "math", "textarea",
];
/// Elements that never have a closing tag.
const VOID: [&str; 14] = [
    "img", "br", "hr", "input", "meta", "link", "source", "wbr", "area", "col", "embed", "param",
    "track", "base",
];

/// A forgiving tag-soup tokenizer: good enough for pulling text out of
/// real-world pages, never fails.
fn tokenize(html: &str) -> Vec<Token<'_>> {
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
fn element_end(tokens: &[Token], start: usize) -> usize {
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

// ---- blocks ----------------------------------------------------------------

/// Elements that start a new block.
const BLOCK_TAGS: [&str; 29] = [
    "p",
    "div",
    "section",
    "article",
    "main",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "li",
    "ul",
    "ol",
    "blockquote",
    "pre",
    "figure",
    "figcaption",
    "table",
    "tr",
    "td",
    "th",
    "dl",
    "dt",
    "dd",
    "hr",
    "header",
    "footer",
    "details",
];

/// Elements that are page chrome, not article content.
const CHROME_TAGS: [&str; 9] = [
    "nav", "aside", "form", "button", "select", "iframe", "footer", "dialog", "menu",
];

/// Prefixes of class/id words that mark boilerplate (sharing bars, comments…).
const CHROME_HINTS: [&str; 20] = [
    "share",
    "social",
    "comment",
    "related",
    "newsletter",
    "subscribe",
    "promo",
    "advert",
    "cookie",
    "sidebar",
    "breadcrumb",
    "byline",
    "author",
    "tags",
    "footer",
    "nav",
    "menu",
    "popup",
    "modal",
    "paywall",
];

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Paragraph,
    Heading,
    Quote,
    Bullet,
    Code,
}

struct Builder<'b> {
    base: Option<&'b Url>,
    blocks: Vec<Block>,
    buf: String,
    kind: Kind,
    images: usize,
}

impl Builder<'_> {
    fn flush(&mut self) {
        let raw = std::mem::take(&mut self.buf);
        let t = if self.kind == Kind::Code {
            let decoded = html_escape::decode_html_entities(&raw);
            decoded.trim_matches('\n').trim_end().to_string()
        } else {
            text::collapse_ws(&html_escape::decode_html_entities(&raw))
        };
        if t.is_empty() {
            return;
        }
        self.blocks.push(match self.kind {
            Kind::Paragraph => Block::Paragraph(t),
            Kind::Heading => Block::Heading(t),
            Kind::Quote => Block::Quote(t),
            Kind::Bullet => Block::Bullet(t),
            Kind::Code => Block::Code(t),
        });
    }

    fn image(&mut self, tag: &str) {
        if self.images >= MAX_IMAGES {
            return;
        }
        let tiny =
            |a: &str| attr(tag, a).is_some_and(|v| v.trim().parse::<u32>().is_ok_and(|n| n <= 2));
        if tiny("width") || tiny("height") {
            return; // tracking pixel
        }
        // Lazy-loading pages keep the real address in data-src.
        let Some(src) = attr(tag, "data-src")
            .or_else(|| attr(tag, "src"))
            .filter(|s| !s.is_empty() && !s.starts_with("data:"))
        else {
            return;
        };
        let src = html_escape::decode_html_entities(&src).into_owned();
        let url = match self.base {
            Some(b) => b.join(&src).map(|u| u.to_string()).unwrap_or(src),
            None => src,
        };
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return;
        }
        self.flush();
        if self.blocks.last() != Some(&Block::Image(url.clone())) {
            self.blocks.push(Block::Image(url));
            self.images += 1;
        }
    }
}

fn is_chrome(name: &str, tag: &str) -> bool {
    if CHROME_TAGS.contains(&name) {
        return true;
    }
    let hints = format!(
        "{} {}",
        attr(tag, "class").unwrap_or_default(),
        attr(tag, "id").unwrap_or_default()
    )
    .to_ascii_lowercase();
    // Word parts, so "share-bar" and "post_comments" both count.
    hints
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| CHROME_HINTS.iter().any(|h| w.starts_with(h)))
        || attr(tag, "aria-hidden").is_some_and(|v| v == "true")
}

fn blocks_from_tokens(tokens: &[Token], base: Option<&Url>, skip_chrome: bool) -> Vec<Block> {
    let mut b = Builder {
        base,
        blocks: Vec::new(),
        buf: String::new(),
        kind: Kind::Paragraph,
        images: 0,
    };
    // Open containers that decide the kind of the text inside them.
    let mut stack: Vec<Kind> = Vec::new();
    let kind_of = |stack: &[Kind]| {
        // The innermost list item or heading wins; quotes and code colour everything inside.
        if stack.contains(&Kind::Code) {
            Kind::Code
        } else {
            stack.last().copied().unwrap_or(Kind::Paragraph)
        }
    };
    let container = |name: &str| match name {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => Some(Kind::Heading),
        "blockquote" => Some(Kind::Quote),
        "li" => Some(Kind::Bullet),
        "pre" => Some(Kind::Code),
        _ => None,
    };

    let mut i = 0;
    while i < tokens.len() && b.blocks.len() < MAX_BLOCKS {
        match &tokens[i] {
            Token::Text(t) => b.buf.push_str(t),
            Token::Open { name, tag } => {
                if skip_chrome && is_chrome(name, tag) {
                    i = element_end(tokens, i);
                    continue;
                }
                if name == "img" {
                    b.image(tag);
                } else if name == "br" {
                    b.buf.push('\n');
                } else if BLOCK_TAGS.contains(&name.as_str()) {
                    b.flush();
                    if let Some(k) = container(name) {
                        stack.push(k);
                    }
                    b.kind = kind_of(&stack);
                }
            }
            Token::Close(name) => {
                if BLOCK_TAGS.contains(&name.as_str()) {
                    b.flush();
                    if let Some(k) = container(name)
                        && let Some(p) = stack.iter().rposition(|s| *s == k)
                    {
                        stack.remove(p);
                    }
                    // Items left open (`<li>a<li>b</ul>`) end with their list.
                    if name == "ul" || name == "ol" {
                        while stack.last() == Some(&Kind::Bullet) {
                            stack.pop();
                        }
                    }
                    b.kind = kind_of(&stack);
                }
            }
        }
        i += 1;
    }
    b.flush();
    b.blocks
}

/// Converts an HTML fragment (a feed entry's content) into blocks.
pub fn blocks_from_html(html: &str, base: Option<&Url>) -> Vec<Block> {
    // Plain-text content (no tags at all) still reads as paragraphs.
    if !html.contains('<') {
        return html
            .split("\n\n")
            .map(text::collapse_ws)
            .filter(|p| !p.is_empty())
            .map(Block::Paragraph)
            .collect();
    }
    blocks_from_tokens(&tokenize(html), base, false)
}

/// Finds the article on a full web page and converts it into blocks.
pub fn extract_article(page: &str, base: &Url) -> Vec<Block> {
    let tokens = tokenize(page);

    // Candidate regions, most specific markup first.
    let mut best: Option<Vec<Block>> = None;
    let mut best_len = 0;
    for (i, t) in tokens.iter().enumerate() {
        let Token::Open { name, tag } = t else {
            continue;
        };
        let class = attr(tag, "class").unwrap_or_default().to_ascii_lowercase();
        let candidate = name == "article"
            || name == "main"
            || attr(tag, "itemprop").is_some_and(|v| v.eq_ignore_ascii_case("articlebody"))
            || [
                "entry-content",
                "post-content",
                "article-content",
                "article-body",
                "post-body",
                "story-body",
            ]
            .iter()
            .any(|c| class.split_whitespace().any(|w| w == *c));
        if !candidate {
            continue;
        }
        let end = element_end(&tokens, i);
        let blocks = blocks_from_tokens(
            &tokens[i + 1..end.saturating_sub(1).max(i + 1)],
            Some(base),
            true,
        );
        let len = text_len(&blocks);
        if len > best_len {
            best_len = len;
            best = Some(blocks);
        }
    }
    if let Some(blocks) = best.filter(|_| best_len >= MIN_REGION_CHARS) {
        return blocks;
    }

    // No recognisable article markup: keep substantial paragraphs from the
    // whole page, plus the headings, quotes and images between them.
    let body = tokens
        .iter()
        .position(|t| matches!(t, Token::Open { name, .. } if name == "body"))
        .unwrap_or(0);
    let all = blocks_from_tokens(&tokens[body..], Some(base), true);
    let substantial =
        |b: &Block| matches!(b, Block::Paragraph(t) if t.chars().count() >= MIN_FALLBACK_PARAGRAPH);
    let (Some(first), Some(last)) = (
        all.iter().position(substantial),
        all.iter().rposition(substantial),
    ) else {
        return Vec::new();
    };
    all[first..=last]
        .iter()
        .filter(
            |b| !matches!(b, Block::Paragraph(t) if t.chars().count() < MIN_FALLBACK_PARAGRAPH / 2),
        )
        .cloned()
        .collect()
}

/// Final touches before display: drops a heading that repeats the article
/// title and an image that repeats the lead image.
pub fn tidy(mut blocks: Vec<Block>, title: &str, lead_image: Option<&str>) -> Vec<Block> {
    let same = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b.trim());
    if let Some(p) = blocks
        .iter()
        .take(4)
        .position(|b| matches!(b, Block::Heading(t) if same(t, title)))
    {
        blocks.remove(p);
    }
    if let Some(lead) = lead_image
        && let Some(p) = blocks
            .iter()
            .take(4)
            .position(|b| matches!(b, Block::Image(u) if u == lead))
    {
        blocks.remove(p);
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn para(s: &str) -> Block {
        Block::Paragraph(s.into())
    }

    #[test]
    fn converts_feed_html_to_blocks() {
        let html = r#"<p>First <b>bold</b> paragraph.</p>
            <h2>A heading</h2>
            <ul><li>One</li><li>Two &amp; three</li></ul>
            <blockquote><p>Quoted</p></blockquote>
            <pre><code>fn main() {
    println!("hi");
}</code></pre>
            <p>Tail<br>line</p>"#;
        assert_eq!(
            blocks_from_html(html, None),
            vec![
                para("First bold paragraph."),
                Block::Heading("A heading".into()),
                Block::Bullet("One".into()),
                Block::Bullet("Two & three".into()),
                Block::Quote("Quoted".into()),
                Block::Code("fn main() {\n    println!(\"hi\");\n}".into()),
                para("Tail line"),
            ]
        );
    }

    #[test]
    fn plain_text_content_becomes_paragraphs() {
        assert_eq!(
            blocks_from_html("One  para.\n\nTwo\npara.", None),
            vec![para("One para."), para("Two para.")]
        );
    }

    #[test]
    fn resolves_images_and_skips_trackers() {
        let base = Url::parse("https://site.org/posts/1").unwrap();
        let html = r#"<p>Hi</p><img src="/a.jpg"><img src="https://t.co/p.gif" width="1" height="1">
            <img data-src="b.png" src="data:image/gif;base64,xx"><img src="/a.jpg">"#;
        assert_eq!(
            blocks_from_html(html, Some(&base)),
            vec![
                para("Hi"),
                Block::Image("https://site.org/a.jpg".into()),
                Block::Image("https://site.org/posts/b.png".into()),
                Block::Image("https://site.org/a.jpg".into()),
            ]
        );
    }

    #[test]
    fn ignores_scripts_styles_and_comments() {
        let html = "<p>A<script>alert('<p>x</p>')</script>B<!-- <p>c</p> --></p><style>p{}</style><p>D</p>";
        assert_eq!(blocks_from_html(html, None), vec![para("AB"), para("D")]);
    }

    #[test]
    fn survives_broken_markup() {
        assert_eq!(
            blocks_from_html("<p>open <b>never closed <p>next < 3 and 4 > 2", None),
            vec![para("open never closed"), para("next < 3 and 4 > 2")]
        );
        assert_eq!(blocks_from_html("<", None), vec![para("<")]);
        assert_eq!(
            blocks_from_html("<ul><li>a<li>b</ul><p>after</p>", None),
            vec![
                Block::Bullet("a".into()),
                Block::Bullet("b".into()),
                para("after")
            ]
        );
        assert!(blocks_from_html("", None).is_empty());
    }

    fn long(word: &str) -> String {
        vec![word; 60].join(" ")
    }

    #[test]
    fn extracts_the_article_region() {
        let base = Url::parse("https://news.org/story").unwrap();
        let page = format!(
            r#"<html><head><title>T</title><script>var x = "<article>";</script></head><body>
            <nav><a href="/">Home</a><p>{nav}</p></nav>
            <article>
              <h1>The Story</h1>
              <div class="share-bar"><p>Share this on social media please</p></div>
              <p>{body1}</p>
              <figure><img src="/img/lead.jpg"><figcaption>Caption</figcaption></figure>
              <p>{body2}</p>
              <aside><p>{aside}</p></aside>
              <section id="comments"><p>{comment}</p></section>
            </article>
            <footer><p>{foot}</p></footer></body></html>"#,
            nav = long("menu"),
            body1 = long("alpha"),
            body2 = long("beta"),
            aside = long("ad"),
            comment = long("rant"),
            foot = long("legal"),
        );
        let blocks = extract_article(&page, &base);
        assert_eq!(
            blocks,
            vec![
                Block::Heading("The Story".into()),
                para(&long("alpha")),
                Block::Image("https://news.org/img/lead.jpg".into()),
                para("Caption"),
                para(&long("beta")),
            ]
        );
    }

    #[test]
    fn prefers_the_richest_candidate() {
        let base = Url::parse("https://x.org/").unwrap();
        let page = format!(
            r#"<body><article><p>Teaser</p></article>
               <div class="entry-content"><p>{}</p><p>{}</p></div></body>"#,
            long("one"),
            long("two")
        );
        assert_eq!(
            extract_article(&page, &base),
            vec![para(&long("one")), para(&long("two"))]
        );
    }

    #[test]
    fn falls_back_to_substantial_paragraphs() {
        let base = Url::parse("https://x.org/").unwrap();
        let page = format!(
            r#"<body><div><p>Log in</p></div><div><p>{}</p><h2>Part two</h2><p>ok</p><p>{}</p></div>
               <div><p>Copyright</p></div></body>"#,
            long("first"),
            long("second")
        );
        assert_eq!(
            extract_article(&page, &base),
            vec![
                para(&long("first")),
                Block::Heading("Part two".into()),
                para(&long("second")),
            ]
        );
        assert!(extract_article("<body><p>tiny</p></body>", &base).is_empty());
    }

    #[test]
    fn storage_round_trip() {
        let blocks = vec![
            Block::Heading("Title \\ slash".into()),
            para("Line"),
            Block::Code("a\nb\\nc".into()),
            Block::Quote("q".into()),
            Block::Bullet("l".into()),
            Block::Image("https://x.org/a.jpg".into()),
        ];
        let s = encode(&blocks);
        assert_eq!(s.lines().count(), blocks.len());
        assert_eq!(decode(&s), blocks);
        // Unknown kinds (from a future version) are skipped, not fatal.
        assert_eq!(decode("z whatever\np ok\n"), vec![para("ok")]);
        assert!(decode("").is_empty());
    }

    #[test]
    fn decides_when_to_fetch_the_page() {
        assert!(needs_full_page(&[para("A short summary…")]));
        assert!(!needs_full_page(&[para(&"word ".repeat(300))]));
        assert!(is_scrapable("https://blog.org/post"));
        assert!(!is_scrapable("https://www.youtube.com/watch?v=x"));
        assert!(!is_scrapable("https://youtu.be/x"));
        assert!(!is_scrapable("mailto:someone@x.org"));
        assert!(!is_scrapable("not a url"));
    }

    #[test]
    fn tidies_repeated_title_and_image() {
        let blocks = vec![
            Block::Heading("My Post".into()),
            Block::Image("https://x.org/lead.jpg".into()),
            para("Body"),
            Block::Heading("My Post".into()),
        ];
        assert_eq!(
            tidy(blocks, " my post ", Some("https://x.org/lead.jpg")),
            vec![para("Body"), Block::Heading("My Post".into())]
        );
    }
}
