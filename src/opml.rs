//! OPML, the subscription list format every feed reader imports and exports:
//! reading the feeds out of one, and writing Aggrega's sources as OPML 2.0.

use std::collections::HashSet;

use anyhow::{Result, bail};
use html_escape::encode_double_quoted_attribute as escape;

use crate::html::{self, Token};

/// One feed in an OPML file.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub title: String,
    pub xml_url: String,
    pub html_url: Option<String>,
}

/// Every feed in `opml`, in document order, each URL once. Folders (outlines
/// holding outlines) are flattened, and outlines without an `xmlUrl` (folders,
/// notes, links) are skipped.
pub fn parse(opml: &str) -> Result<Vec<Source>> {
    let tokens = html::tokenize(opml);
    if !tokens
        .iter()
        .any(|t| matches!(t, Token::Open { name, .. } if name == "opml"))
    {
        bail!("that file isn't an OPML subscription list");
    }
    let value = |tag: &str, name: &str| {
        html::attr(tag, name)
            .map(|v| html_escape::decode_html_entities(&v).trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let mut seen = HashSet::new();
    let mut sources = Vec::new();
    for t in &tokens {
        let Token::Open { name, tag } = t else {
            continue;
        };
        if name != "outline" {
            continue;
        }
        // `attr` matches names case-insensitively, so this also reads `xmlURL`.
        let Some(xml_url) = value(tag, "xmlurl") else {
            continue;
        };
        if !seen.insert(xml_url.clone()) {
            continue;
        }
        sources.push(Source {
            title: value(tag, "title")
                .or_else(|| value(tag, "text"))
                .unwrap_or_else(|| xml_url.clone()),
            html_url: value(tag, "htmlurl"),
            xml_url,
        });
    }
    Ok(sources)
}

/// An OPML 2.0 document listing `sources`. `created` is an RFC 822 date.
pub fn write(sources: &[Source], created: &str) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <opml version=\"2.0\">\n  \
         <head>\n    \
         <title>Aggrega sources</title>\n",
    );
    out += &format!("    <dateCreated>{}</dateCreated>\n", escape(created));
    out += "  </head>\n  <body>\n";
    for s in sources {
        let title = escape(&s.title);
        out += &format!(
            "    <outline type=\"rss\" text=\"{title}\" title=\"{title}\" xmlUrl=\"{}\"",
            escape(&s.xml_url)
        );
        if let Some(site) = &s.html_url {
            out += &format!(" htmlUrl=\"{}\"", escape(site));
        }
        out += "/>\n";
    }
    out += "  </body>\n</opml>\n";
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real-world exports (trimmed), one per reader.
    fn fixture(name: &str) -> Vec<Source> {
        let path = format!("{}/tests/fixtures/opml/{name}", env!("CARGO_MANIFEST_DIR"));
        parse(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn urls(sources: &[Source]) -> Vec<&str> {
        sources.iter().map(|s| s.xml_url.as_str()).collect()
    }

    #[test]
    fn reads_feedly_folders_once_per_feed() {
        let s = fixture("feedly.opml");
        // The Verge sits in two folders.
        assert_eq!(
            urls(&s),
            [
                "https://www.theverge.com/rss/index.xml",
                "https://feeds.arstechnica.com/arstechnica/index",
                "https://feeds.bbci.co.uk/news/rss.xml",
            ]
        );
        assert_eq!(s[1].title, "Ars Technica");
        assert_eq!(s[1].html_url.as_deref(), Some("https://arstechnica.com"));
    }

    #[test]
    fn reads_inoreader_with_escaped_titles() {
        let s = fixture("inoreader.opml");
        assert_eq!(urls(&s).len(), 3);
        assert_eq!(s[1].title, "Simon Willison's Weblog");
        assert_eq!(s[2].title, "This Week in Rust");
    }

    #[test]
    fn reads_netnewswire_top_level_and_nested() {
        let s = fixture("netnewswire.opml");
        assert_eq!(
            urls(&s),
            [
                "https://daringfireball.net/feeds/main",
                "https://sixcolors.com/feed/",
                "https://netnewswire.blog/feed.json",
            ]
        );
        assert_eq!(s[0].title, "Daring Fireball");
    }

    #[test]
    fn reads_miniflux_and_decodes_urls() {
        let s = fixture("miniflux.opml");
        assert_eq!(
            urls(&s),
            [
                "https://lwn.net/headlines/rss",
                "https://hnrss.org/frontpage?points=100&comments=25",
            ]
        );
        assert_eq!(s[1].title, "Hacker News: Front Page");
    }

    #[test]
    fn skips_outlines_without_a_feed() {
        let s = parse(
            r#"<opml version="2.0"><body>
                <outline text="Folder"/>
                <outline type="link" text="A page" url="https://example.com/"/>
                <outline text="Untitled" xmlURL="https://example.com/feed"/>
                <outline xmlUrl="https://example.org/rss"/>
                <outline text="Blank" xmlUrl="  "/>
            </body></opml>"#,
        )
        .unwrap();
        assert_eq!(
            s,
            [
                Source {
                    title: "Untitled".into(),
                    xml_url: "https://example.com/feed".into(),
                    html_url: None,
                },
                // No title or text: fall back to the URL.
                Source {
                    title: "https://example.org/rss".into(),
                    xml_url: "https://example.org/rss".into(),
                    html_url: None,
                },
            ]
        );
    }

    #[test]
    fn rejects_files_that_are_not_opml() {
        assert!(parse("<rss><channel><title>x</title></channel></rss>").is_err());
        assert!(parse("just some text").is_err());
        assert_eq!(parse("<opml><body></body></opml>").unwrap(), []);
    }

    #[test]
    fn writes_what_it_reads() {
        let sources = vec![
            Source {
                title: "Q&A \"weekly\" <news>".into(),
                xml_url: "https://example.com/feed?a=1&b=2".into(),
                html_url: Some("https://example.com/".into()),
            },
            Source {
                title: "Città".into(),
                xml_url: "https://example.org/rss".into(),
                html_url: None,
            },
        ];
        let doc = write(&sources, "Sun, 04 Oct 2026 10:00:00 +0000");
        assert!(
            doc.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<opml version=\"2.0\">")
        );
        assert!(doc.contains("<dateCreated>Sun, 04 Oct 2026 10:00:00 +0000</dateCreated>"));
        assert!(doc.contains("xmlUrl=\"https://example.com/feed?a=1&amp;b=2\""));
        // Markup in titles is escaped, not written as elements.
        assert!(!doc.contains("<news>"));
        assert_eq!(parse(&doc).unwrap(), sources);
    }
}
