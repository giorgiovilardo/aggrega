//! Interface language: English (the source strings) or Italian.
//!
//! Slint strings are marked with `@tr(…)` and translated from
//! `translations/<lang>/LC_MESSAGES/aggrega.po`, bundled in at build time.
//! Strings built in Rust go through `tr!` / `trn!`, which look them up in the
//! same catalog, so one file holds every translation. Placeholders follow
//! Slint's: `{}` in order, `{0}`, `{1}`… by position, and `{n}` for the count
//! of a plural.
//!
//! The language is per thread, like Slint's own: translate on the UI thread.

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt::{Display, Write};
use std::sync::LazyLock;

use chrono::{DateTime, Datelike, Local};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Lang {
    #[default]
    En,
    It,
}

impl Lang {
    /// The code saved in settings and shown on the language switch.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::It => "it",
        }
    }

    pub fn from_code(code: &str) -> Option<Lang> {
        match code {
            "en" => Some(Lang::En),
            "it" => Some(Lang::It),
            _ => None,
        }
    }

    fn catalog(self) -> Option<&'static Catalog> {
        match self {
            Lang::En => None,
            Lang::It => Some(&IT),
        }
    }
}

thread_local! {
    static LANG: Cell<Lang> = const { Cell::new(Lang::En) };
}

pub fn lang() -> Lang {
    LANG.with(Cell::get)
}

/// Switches this thread's strings, Slint's included. Needs the window to
/// exist (it registers the bundled translations).
pub fn set_lang(lang: Lang) {
    LANG.with(|l| l.set(lang));
    if let Err(e) = slint::select_bundled_translation(lang.code()) {
        eprintln!("aggrega: couldn't switch the interface language: {e}");
    }
}

const IT_PO: &str = include_str!("../translations/it/LC_MESSAGES/aggrega.po");
static IT: LazyLock<Catalog> = LazyLock::new(|| Catalog::parse(IT_PO));

/// `tr!("Removed {}", title)`: the current language's version of a message.
macro_rules! tr {
    ($msg:literal $(, $arg:expr)* $(,)?) => {
        $crate::i18n::translate($msg, &[$(&$arg as &dyn ::std::fmt::Display),*])
    };
}

/// `trn!(n, "{n} source", "{n} sources")`: the form that fits `n`.
macro_rules! trn {
    ($n:expr, $one:literal, $many:literal $(, $arg:expr)* $(,)?) => {
        $crate::i18n::translate_plural($n as u64, $one, $many, &[$(&$arg as &dyn ::std::fmt::Display),*])
    };
}

pub(crate) use {tr, trn};

pub fn translate(msg: &str, args: &[&dyn Display]) -> String {
    let template = lang().catalog().and_then(|c| c.get(msg, 0)).unwrap_or(msg);
    format(template, None, args)
}

pub fn translate_plural(n: u64, one: &str, many: &str, args: &[&dyn Display]) -> String {
    // English and Italian both use the singular for exactly one.
    let form = usize::from(n != 1);
    let template = lang()
        .catalog()
        .and_then(|c| c.get(one, form))
        .unwrap_or(if form == 0 { one } else { many });
    format(template, Some(n), args)
}

/// Fills `{}`, `{0}` and `{n}` placeholders; `{{` and `}}` are literal braces.
fn format(template: &str, n: Option<u64>, args: &[&dyn Display]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut next = 0;
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut key = String::new();
                for k in chars.by_ref() {
                    if k == '}' {
                        break;
                    }
                    key.push(k);
                }
                if key == "n" {
                    if let Some(n) = n {
                        let _ = write!(out, "{n}");
                    }
                    continue;
                }
                let index = if key.is_empty() {
                    next += 1;
                    next - 1
                } else {
                    key.parse().unwrap_or(usize::MAX)
                };
                if let Some(arg) = args.get(index) {
                    let _ = write!(out, "{arg}");
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// A gettext `.po` file: each message id with its translation, or its plural
/// forms. Untranslated (empty) entries are left out, so they fall back to
/// English.
struct Catalog(HashMap<String, Vec<String>>);

impl Catalog {
    fn parse(po: &str) -> Catalog {
        #[derive(PartialEq)]
        enum Field {
            None,
            Id,
            Plural,
            Str(usize),
        }
        let mut map = HashMap::new();
        let mut id = String::new();
        let mut strs: Vec<String> = Vec::new();
        let mut field = Field::None;
        let mut flush = |id: &mut String, strs: &mut Vec<String>| {
            if !id.is_empty() && strs.iter().all(|s| !s.is_empty()) && !strs.is_empty() {
                map.insert(std::mem::take(id), std::mem::take(strs));
            }
            id.clear();
            strs.clear();
        };
        for line in po.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = match line.split_once(' ') {
                Some((k, v)) if !line.starts_with('"') => (k, unquote(v)),
                _ => ("", unquote(line)),
            };
            match key {
                "msgctxt" => field = Field::None,
                "msgid" => {
                    flush(&mut id, &mut strs);
                    field = Field::Id;
                    id = value;
                }
                "msgid_plural" => field = Field::Plural,
                "msgstr" => {
                    field = Field::Str(0);
                    strs = vec![value];
                }
                k if k.starts_with("msgstr[") => {
                    let i: usize = k[7..k.len() - 1].parse().unwrap_or(0);
                    if strs.len() <= i {
                        strs.resize(i + 1, String::new());
                    }
                    strs[i] = value;
                    field = Field::Str(i);
                }
                // A continuation line: append to whatever came before.
                _ => match field {
                    Field::Id => id += &value,
                    Field::Str(i) => strs[i] += &value,
                    Field::Plural | Field::None => {}
                },
            }
        }
        flush(&mut id, &mut strs);
        Catalog(map)
    }

    fn get(&self, msgid: &str, form: usize) -> Option<&str> {
        let forms = self.0.get(msgid)?;
        forms.get(form).or(forms.last()).map(String::as_str)
    }
}

/// The contents of a quoted `.po` string, with escapes resolved.
fn unquote(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix('"').unwrap_or(s);
    let s = s.strip_suffix('"').unwrap_or(s);
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(c) => out.push(c),
            None => {}
        }
    }
    out
}

const MONTHS_EN: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const MONTHS_IT: [&str; 12] = [
    "gennaio",
    "febbraio",
    "marzo",
    "aprile",
    "maggio",
    "giugno",
    "luglio",
    "agosto",
    "settembre",
    "ottobre",
    "novembre",
    "dicembre",
];
const WEEKDAYS_EN: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];
const WEEKDAYS_IT: [&str; 7] = [
    "lunedì",
    "martedì",
    "mercoledì",
    "giovedì",
    "venerdì",
    "sabato",
    "domenica",
];

/// The masthead date: "Friday, October 9, 2026" / "venerdì 9 ottobre 2026".
pub fn long_date(t: DateTime<Local>) -> String {
    let (day, month) = (
        t.weekday().num_days_from_monday() as usize,
        t.month0() as usize,
    );
    match lang() {
        Lang::En => format!(
            "{}, {} {}, {}",
            WEEKDAYS_EN[day],
            MONTHS_EN[month],
            t.day(),
            t.year()
        ),
        Lang::It => format!(
            "{} {} {} {}",
            WEEKDAYS_IT[day],
            t.day(),
            MONTHS_IT[month],
            t.year()
        ),
    }
}

/// An older article's date: "Oct 9" / "9 ott", with the year if it isn't this one.
pub fn short_date(t: DateTime<Local>, with_year: bool) -> String {
    let month = t.month0() as usize;
    let s = match lang() {
        Lang::En => format!("{} {}", &MONTHS_EN[month][..3], t.day()),
        Lang::It => format!("{} {}", t.day(), &MONTHS_IT[month][..3]),
    };
    match (with_year, lang()) {
        (false, _) => s,
        (true, Lang::En) => format!("{s}, {}", t.year()),
        (true, Lang::It) => format!("{s} {}", t.year()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Switches only the Rust strings: tests run without a window.
    fn with_lang<T>(lang: Lang, f: impl FnOnce() -> T) -> T {
        LANG.with(|l| l.set(lang));
        let out = f();
        LANG.with(|l| l.set(Lang::En));
        out
    }

    #[test]
    fn formats_placeholders() {
        assert_eq!(format("{} and {}", None, &[&1, &"two"]), "1 and two");
        assert_eq!(format("{1} before {0}", None, &[&"a", &"b"]), "b before a");
        assert_eq!(
            format("{n} left, {{literal}}", Some(3), &[]),
            "3 left, {literal}"
        );
    }

    #[test]
    fn parses_po_entries() {
        let c = Catalog::parse(
            r#"
msgid ""
msgstr ""
"Plural-Forms: nplurals=2; plural=(n != 1);\n"

# A comment
msgid "Removed {}"
msgstr "Rimossa {}"

msgid "long"
" message"
msgstr "messaggio "
"lungo \"citato\""

msgid "{n} source"
msgid_plural "{n} sources"
msgstr[0] "{n} fonte"
msgstr[1] "{n} fonti"

msgid "Untranslated"
msgstr ""
"#,
        );
        assert_eq!(c.get("Removed {}", 0), Some("Rimossa {}"));
        assert_eq!(c.get("long message", 0), Some("messaggio lungo \"citato\""));
        assert_eq!(c.get("{n} source", 1), Some("{n} fonti"));
        assert_eq!(c.get("Untranslated", 0), None);
        assert_eq!(c.get("", 0), None, "the header isn't a message");
    }

    #[test]
    fn translates_rust_strings() {
        assert_eq!(tr!("Removed {}", "Blog"), "Removed Blog");
        assert_eq!(trn!(2, "{n} source", "{n} sources"), "2 sources");
        with_lang(Lang::It, || {
            assert_eq!(tr!("All articles"), "Tutti gli articoli");
            assert_eq!(trn!(1, "{n} source", "{n} sources"), "1 fonte");
            assert_eq!(trn!(3, "{n} source", "{n} sources"), "3 fonti");
            assert_eq!(tr!("Not in the catalog {}", 1), "Not in the catalog 1");
        });
    }

    #[test]
    fn dates_follow_the_language() {
        let t = Local.with_ymd_and_hms(2026, 10, 9, 12, 0, 0).unwrap();
        assert_eq!(long_date(t), "Friday, October 9, 2026");
        assert_eq!(short_date(t, true), "Oct 9, 2026");
        with_lang(Lang::It, || {
            assert_eq!(long_date(t), "venerdì 9 ottobre 2026");
            assert_eq!(short_date(t, false), "9 ott");
        });
    }

    /// Every string marked for translation, in the UI or in Rust, has an
    /// Italian version with the same placeholders.
    #[test]
    fn every_marked_string_is_translated() {
        let root = env!("CARGO_MANIFEST_DIR");
        let mut missing = Vec::new();
        let mut check = |file: &str, msgid: String| match IT.get(&msgid, 0) {
            None => missing.push(format!("{file}: {msgid:?}")),
            Some(it) if placeholders(it) != placeholders(&msgid) => {
                missing.push(format!("{file}: placeholders differ in {msgid:?}"))
            }
            Some(_) => {}
        };
        for dir in ["ui", "src"] {
            for entry in std::fs::read_dir(format!("{root}/{dir}")).unwrap() {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                if name == "i18n.rs" {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                for marker in ["@tr(", "tr!(", "trn!("] {
                    for (at, _) in text.match_indices(marker) {
                        let rest = &text[at + marker.len()..];
                        let Some(open) = rest.find('"') else { continue };
                        if let Some(msgid) = literal(&rest[open..]) {
                            check(&name, msgid);
                        }
                    }
                }
            }
        }
        assert!(missing.is_empty(), "untranslated:\n{}", missing.join("\n"));
    }

    /// The string literal `s` starts with, unescaped.
    fn literal(s: &str) -> Option<String> {
        let mut out = String::new();
        let mut chars = s.chars().skip(1);
        while let Some(c) = chars.next() {
            match c {
                '"' => return Some(out),
                '\\' => match chars.next()? {
                    'n' => out.push('\n'),
                    c => out.push(c),
                },
                c => out.push(c),
            }
        }
        None
    }

    /// Placeholder names, with positions dropped: a translation may reorder.
    fn placeholders(s: &str) -> Vec<String> {
        let mut found: Vec<String> = s
            .split('{')
            .skip(1)
            .filter_map(|p| {
                p.split_once('}')
                    .map(|(k, _)| k.replace(|c: char| c.is_ascii_digit(), ""))
            })
            .collect();
        found.sort();
        found
    }
}
