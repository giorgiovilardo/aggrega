//! Feeds as data: what a refresh asks for and gets back, and the offline
//! half of fetching, which turns a downloaded feed (or the HTML page
//! advertising one) into articles ready to store.

use anyhow::{Context, Result};
use feed_rs::model::Entry;
use url::Url;

use crate::{html, reader, text};

const SNIPPET_CHARS: usize = 280;

/// An article parsed from a feed, ready to be stored.
#[derive(Debug, Clone)]
pub struct NewArticle {
    pub guid: String,
    pub title: String,
    pub link: String,
    pub snippet: String,
    pub image_url: Option<String>,
    pub published: i64,
    /// Reader view blocks from the feed's own content (`reader::encode`d).
    pub body: String,
}

/// A successfully downloaded and parsed feed.
#[derive(Debug)]
pub struct Fetched {
    pub title: String,
    pub site_url: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub articles: Vec<NewArticle>,
}

/// What we need to know to (conditionally) refresh one subscription.
#[derive(Debug, Clone)]
pub struct FeedJob {
    pub id: i64,
    pub url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// `Ok(None)` means the server answered "304 Not Modified".
pub type FetchResult = Result<Option<Fetched>>;

/// Parses an RSS, Atom or JSON feed downloaded from `url`.
pub fn parse(url: &str, body: &[u8]) -> Result<Fetched> {
    let feed = feed_rs::parser::Builder::new()
        .base_uri(Some(url))
        .build()
        .parse(body)
        .context("not a valid RSS/Atom feed")?;

    let now = chrono::Utc::now().timestamp();
    let title = feed
        .title
        .map(|t| html::to_text(&t.content))
        .filter(|t| !t.is_empty())
        .or_else(|| {
            Url::parse(url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned))
        })
        .unwrap_or_else(|| url.to_string());
    let site_url = feed
        .links
        .iter()
        .find(|l| l.rel.as_deref().is_none_or(|r| r == "alternate"))
        .map(|l| l.href.clone());

    let articles = feed
        .entries
        .iter()
        .filter_map(|e| convert_entry(e, now))
        .collect();
    Ok(Fetched {
        title,
        site_url,
        etag: None,
        last_modified: None,
        articles,
    })
}

fn convert_entry(e: &Entry, now: i64) -> Option<NewArticle> {
    let link = e
        .links
        .iter()
        .find(|l| l.rel.as_deref().is_none_or(|r| r == "alternate"))
        .or_else(|| e.links.first())
        .map(|l| l.href.clone())
        .or_else(|| e.id.starts_with("http").then(|| e.id.clone()))?;

    let summary_html = e
        .summary
        .as_ref()
        .map(|s| s.content.as_str())
        .or_else(|| e.content.as_ref().and_then(|c| c.body.as_deref()))
        .unwrap_or("");
    let content_html = e
        .content
        .as_ref()
        .and_then(|c| c.body.as_deref())
        .unwrap_or("");

    let mut title = e
        .title
        .as_ref()
        .map(|t| html::to_text(&t.content))
        .unwrap_or_default();
    let snippet = text::truncate(&html::to_text(summary_html), SNIPPET_CHARS);
    if title.is_empty() {
        title = if snippet.is_empty() {
            "(untitled)".into()
        } else {
            text::truncate(&snippet, 90)
        };
    }

    let published = e
        .published
        .or(e.updated)
        .map(|d| d.timestamp())
        .unwrap_or(now)
        .min(now); // clamp bogus future dates

    // Relative URLs resolve against the content's xml:base, else the link.
    let base = Url::parse(&link)
        .ok()
        .map(|l| e.base.as_deref().and_then(|b| l.join(b).ok()).unwrap_or(l));
    let image_url = find_image(e, summary_html, content_html).and_then(|src| {
        base.as_ref()
            .and_then(|base| base.join(&src).ok())
            .map(|u| u.to_string())
            .or(Some(src))
    });

    // The reader wants the fullest version the feed offers.
    let full_html = if content_html.len() > summary_html.len() {
        content_html
    } else {
        summary_html
    };
    let body = reader::encode(&reader::blocks_from_html(full_html, base.as_ref()));

    let guid = if e.id.is_empty() {
        link.clone()
    } else {
        e.id.clone()
    };
    Some(NewArticle {
        guid,
        title,
        link,
        snippet,
        image_url,
        published,
        body,
    })
}

/// The entry's thumbnail: a media thumbnail or image, then an image
/// enclosure, then the first real `<img>` in its HTML.
fn find_image(e: &Entry, summary_html: &str, content_html: &str) -> Option<String> {
    for m in &e.media {
        if let Some(t) = m.thumbnails.first() {
            return Some(t.image.uri.clone());
        }
        for c in &m.content {
            let is_image = c
                .content_type
                .as_ref()
                .is_some_and(|t| t.to_string().starts_with("image/"))
                || c.url.as_ref().is_some_and(|u| looks_like_image(u.as_str()));
            if is_image && let Some(u) = &c.url {
                return Some(u.to_string());
            }
        }
    }
    for l in &e.links {
        if l.rel.as_deref() == Some("enclosure")
            && (l
                .media_type
                .as_deref()
                .is_some_and(|t| t.starts_with("image/"))
                || looks_like_image(&l.href))
        {
            return Some(l.href.clone());
        }
    }
    first_img_src(summary_html).or_else(|| first_img_src(content_html))
}

fn looks_like_image(u: &str) -> bool {
    let path = u
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    [".jpg", ".jpeg", ".png", ".webp", ".gif"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

fn first_img_src(html: &str) -> Option<String> {
    html::open_tags(html, "img")
        // Skip tracking pixels.
        .filter(|tag| !html::attr(tag, "width").is_some_and(|w| w.trim() == "1"))
        .find_map(|tag| html::attr(tag, "src").filter(|s| !s.starts_with("data:")))
        .map(|src| html_escape::decode_html_entities(&src).into_owned())
}

/// Finds feed URLs advertised by an HTML page (`<link rel="alternate">`).
pub fn discover_links(page: &str, base: &Url) -> Vec<String> {
    html::open_tags(page, "link")
        .filter(|tag| {
            let rel = html::attr(tag, "rel")
                .unwrap_or_default()
                .to_ascii_lowercase();
            let ty = html::attr(tag, "type")
                .unwrap_or_default()
                .to_ascii_lowercase();
            rel.contains("alternate")
                && (ty.contains("rss") || ty.contains("atom") || ty.contains("feed+json"))
        })
        .filter_map(|tag| html::attr(tag, "href"))
        .filter_map(|href| base.join(&html_escape::decode_html_entities(&href)).ok())
        .map(|u| u.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! fixtures {
        ($($name:literal),* $(,)?) => {
            &[$(($name, include_bytes!(concat!("../tests/fixtures/feeds/", $name)))),*]
        };
    }

    /// Sample feeds from `tests/fixtures/feeds` (see the README there).
    const FIXTURES: &[(&str, &[u8])] = fixtures![
        "atom_mediarss_youtube_1.xml",
        "atom_spec_1.xml",
        "atom_xml_base.xml",
        "jsonfeed_spec_1.json",
        "rss_0.91_spec_1.xml",
        "rss_0.92_spec_1.xml",
        "rss_1.0_spec_1.xml",
        "rss_2.0_bbc.xml",
        "rss_2.0_spec_1.xml",
        "rss_2.0_vimeo_media.xml",
    ];

    fn first_article(name: &str) -> NewArticle {
        let (_, xml) = FIXTURES.iter().find(|(n, _)| *n == name).unwrap();
        let f = parse("https://example.org/feed", xml).unwrap();
        f.articles.into_iter().next().unwrap()
    }

    #[test]
    fn parses_every_fixture() {
        // Parses fine but yields no articles: its items have no link.
        let empty = ["rss_0.92_spec_1.xml"];
        for &(name, xml) in FIXTURES {
            let f =
                parse("https://example.org/feed", xml).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            assert_eq!(f.articles.is_empty(), empty.contains(&name), "{name}");
            for a in &f.articles {
                assert!(!a.title.is_empty() && !a.link.is_empty(), "{name}: {a:?}");
            }
        }
    }

    #[test]
    fn picks_media_thumbnails_but_not_audio() {
        assert_eq!(
            first_article("atom_mediarss_youtube_1.xml")
                .image_url
                .as_deref(),
            Some("https://i1.ytimg.com/vi/0A1ouV7iD8o/hqdefault.jpg")
        );
        // A podcast episode's audio enclosure is not a picture.
        assert_eq!(first_article("rss_2.0_bbc.xml").image_url, None);
    }

    #[test]
    fn links_fall_back_to_the_entry_id() {
        // The entry has no <link>, only an http <id>.
        let a = first_article("atom_xml_base.xml");
        assert_eq!(a.link, "https://numi.st/post/2022/travel-uke");
        assert_eq!(a.guid, a.link);
        // Its content's xml:base is a directory below that link.
        let pic = "https://numi.st/post/2022/travel-uke/IMG_1232.jpeg";
        assert_eq!(a.image_url.as_deref(), Some(pic));
        assert_eq!(
            reader::decode(&a.body),
            vec![reader::Block::Image(pic.into())]
        );
    }

    fn rss(items: &str) -> Vec<u8> {
        format!(
            r#"<?xml version="1.0"?>
            <rss version="2.0" xmlns:content="http://purl.org/rss/1.0/modules/content/"
                 xmlns:media="http://search.yahoo.com/mrss/"><channel>
            <title>Demo</title><link>https://demo.org</link>{items}</channel></rss>"#
        )
        .into_bytes()
    }

    #[test]
    fn parses_rss() {
        let xml = rss(
            r#"<item><title>Hello &amp; welcome</title><link>https://demo.org/1</link><guid>1</guid>
            <description>&lt;p&gt;Body &lt;img src="/pic.jpg"&gt;&lt;/p&gt;</description>
            <pubDate>Tue, 01 Sep 2026 10:00:00 GMT</pubDate></item>"#,
        );
        let f = parse("https://demo.org/rss", &xml).unwrap();
        assert_eq!(f.title, "Demo");
        assert_eq!(f.site_url.as_deref(), Some("https://demo.org/"));
        assert_eq!(f.articles.len(), 1);
        let a = &f.articles[0];
        assert_eq!(a.guid, "1");
        assert_eq!(a.title, "Hello & welcome");
        assert_eq!(a.snippet, "Body");
        assert_eq!(a.published, 1_788_256_800);
        assert_eq!(a.image_url.as_deref(), Some("https://demo.org/pic.jpg"));
        assert_eq!(
            reader::decode(&a.body),
            vec![
                reader::Block::Paragraph("Body".into()),
                reader::Block::Image("https://demo.org/pic.jpg".into()),
            ]
        );
        assert!(parse("https://demo.org/rss", b"<html>not a feed</html>").is_err());
    }

    #[test]
    fn titles_keep_a_literal_less_than() {
        // The XML parser decodes `&lt;`, so the title reaches `html::to_text`
        // as `1 < 2 matters`; it used to come out as just "1".
        let xml = rss(
            r#"<item><title>1 &lt; 2 matters</title><link>https://demo.org/1</link>
            <description>I &lt;3 feeds</description></item>"#,
        );
        let a = &parse("https://demo.org/rss", &xml).unwrap().articles[0];
        assert_eq!(a.title, "1 < 2 matters");
        // Summaries are HTML, so `&lt;` there is a real `<`, still text.
        assert_eq!(a.snippet, "I <3 feeds");
    }

    #[test]
    fn reader_body_prefers_full_content() {
        let xml = rss(
            r#"<item><title>Long read</title><link>https://demo.org/2</link><guid>2</guid>
            <description>Short teaser</description>
            <content:encoded><![CDATA[<h2>Intro</h2><p>The whole story.</p>]]></content:encoded>
            </item>"#,
        );
        let f = parse("https://demo.org/rss", &xml).unwrap();
        let a = &f.articles[0];
        assert_eq!(a.snippet, "Short teaser");
        assert_eq!(
            reader::decode(&a.body),
            vec![
                reader::Block::Heading("Intro".into()),
                reader::Block::Paragraph("The whole story.".into()),
            ]
        );
    }

    #[test]
    fn fills_in_missing_titles_guids_and_dates() {
        let xml = rss(r#"<item><link>https://demo.org/a</link>
                <description>No title, so the snippet stands in</description>
                <pubDate>Fri, 01 Jan 2100 00:00:00 GMT</pubDate></item>
            <item><link>https://demo.org/b</link></item>
            <item><title>No link, no article</title></item>"#);
        let before = chrono::Utc::now().timestamp();
        let f = parse("https://demo.org/rss", &xml).unwrap();
        assert_eq!(f.articles.len(), 2, "entries without a link are skipped");
        let (a, b) = (&f.articles[0], &f.articles[1]);
        assert_eq!(a.title, "No title, so the snippet stands in");
        assert_eq!(b.title, "(untitled)");
        // Items without a guid still get distinct ones, and the same ones on
        // every refresh, or they'd be stored again as new articles each time.
        assert_ne!(a.guid, b.guid);
        let again = parse("https://demo.org/rss", &xml).unwrap();
        assert_eq!(again.articles[0].guid, a.guid);
        assert_eq!(again.articles[1].guid, b.guid);
        // A date in the future is clamped to now, and no date means now.
        assert!(a.published >= before && a.published <= chrono::Utc::now().timestamp());
        assert!(b.published >= before);

        // A feed without a title is named after its host.
        let untitled = br#"<?xml version="1.0"?><rss version="2.0"><channel></channel></rss>"#;
        assert_eq!(
            parse("https://www.site.org/rss", untitled).unwrap().title,
            "www.site.org"
        );
    }

    #[test]
    fn picks_the_best_thumbnail() {
        let image_of = |item: &str| {
            let xml = rss(&format!(
                "<item><link>https://demo.org/p/1</link>{item}</item>"
            ));
            parse("https://demo.org/rss", &xml).unwrap().articles[0]
                .image_url
                .clone()
        };
        let body = r#"<description>&lt;img src="/in-body.jpg"&gt;</description>"#;
        // A media thumbnail beats an enclosure, which beats an <img> in the body.
        assert_eq!(
            image_of(&format!(
                r#"<media:thumbnail url="https://cdn.org/thumb.jpg"/>
                   <enclosure url="https://cdn.org/enc.jpg" type="image/jpeg" length="1"/>{body}"#
            ))
            .as_deref(),
            Some("https://cdn.org/thumb.jpg")
        );
        assert_eq!(
            image_of(&format!(
                r#"<enclosure url="https://cdn.org/enc.jpg" type="image/jpeg" length="1"/>{body}"#
            ))
            .as_deref(),
            Some("https://cdn.org/enc.jpg")
        );
        // Audio enclosures aren't thumbnails; relative body images resolve against the link.
        assert_eq!(
            image_of(&format!(
                r#"<enclosure url="https://cdn.org/ep.mp3" type="audio/mpeg" length="1"/>{body}"#
            ))
            .as_deref(),
            Some("https://demo.org/in-body.jpg")
        );
        // Tracking pixels and inline data are skipped in favour of the next image.
        assert_eq!(
            image_of(
                r#"<description>&lt;img src="https://t.co/px.gif" width="1"&gt;
                   &lt;img src="data:image/png;base64,xx"&gt;
                   &lt;img src="real.jpg?a=1&amp;amp;b=2"&gt;</description>"#
            )
            .as_deref(),
            Some("https://demo.org/p/real.jpg?a=1&b=2")
        );
        // Lazy-loading pages hide the real image behind a placeholder, with a
        // plain copy in <noscript>.
        assert_eq!(
            image_of(
                r#"<description>&lt;img src="data:image/svg+xml,x" data-lazy-src="/lazy.jpg"&gt;
                   &lt;noscript&gt;&lt;img src="/lazy.jpg"&gt;&lt;/noscript&gt;</description>"#
            )
            .as_deref(),
            Some("https://demo.org/lazy.jpg")
        );
        assert_eq!(image_of("<description>Just text</description>"), None);
    }

    #[test]
    fn discovers_feed_links() {
        let page = r#"<html><head><link rel="stylesheet" href="a.css">
            <link rel="alternate" type="application/atom+xml" href="/atom.xml">
            <link rel="alternate" type="text/html" hreflang="de" href="/de/">
            <!-- <link rel="alternate" type="application/rss+xml" href="/old.rss"> -->
            <LINK REL="Alternate" TYPE="application/rss+xml" HREF="rss?a=1&amp;b=2"></head></html>"#;
        let base = Url::parse("https://example.com/blog/").unwrap();
        assert_eq!(
            discover_links(page, &base),
            vec![
                "https://example.com/atom.xml".to_string(),
                "https://example.com/blog/rss?a=1&b=2".to_string(),
            ]
        );
    }
}
