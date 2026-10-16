//! Pure parsing and naming for clipping files under `.hulder/clippings/`:
//! front matter, slugs, file and asset names, and the caps the store
//! enforces. No I/O and no gpui so `saga` and `bifrost` can reuse it.

use anyhow::{Context as _, Result, anyhow};

/// Directory under `.hulder/` that holds the clippings.
pub const DIR: &str = "clippings";
/// Directory under `clippings/` that holds screenshots and other binaries.
pub const ASSETS_DIR: &str = "assets";
pub const SLUG_MAX_LEN: usize = 48;
/// A date prefix plus a dash plus the slug.
pub const DATE_PREFIX_LEN: usize = "2026-10-13-".len();
/// Largest Markdown body we write or read; a page can be big, a book cannot.
pub const MAX_MARKDOWN_BYTES: u64 = 1024 * 1024;
/// Largest asset (screenshot) we write.
pub const MAX_ASSET_BYTES: u64 = 8 * 1024 * 1024;
/// Listing stops after this many entries so a hostile folder cannot stall
/// the panel or an agent.
pub const MAX_LISTED_FILES: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Pick,
    Fetch,
    Pdf,
    /// A request and its redacted response saved from Raven (spec 18.2).
    Raven,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Pick, Kind::Fetch, Kind::Pdf, Kind::Raven];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Pick => "pick",
            Kind::Fetch => "fetch",
            Kind::Pdf => "pdf",
            Kind::Raven => "raven",
        }
    }

    /// Whether `source` is a URL (else a file path).
    pub fn source_is_url(self) -> bool {
        !matches!(self, Kind::Pdf)
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == text.trim())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clipping {
    /// A URL for picks and fetches, an absolute path for PDFs.
    pub source: String,
    /// RFC 3339 UTC timestamp of the capture.
    pub captured: String,
    pub kind: Kind,
    /// The CSS selector of a picked element; `None` for the other kinds.
    pub selector: Option<String>,
    /// The URL had a query string (or fragment) that was stripped from
    /// `source`, since queries carry tokens and keys.
    pub query_present: bool,
    /// Further front matter lines a producer adds (Raven's `request` and
    /// `status`), kept in order. Keys are `[a-z0-9_]` and never one of the
    /// fixed keys; anything else is dropped on write.
    pub extra: Vec<(String, String)>,
    /// Everything after the closing `---`, verbatim.
    pub body: String,
}

const FIXED_KEYS: [&str; 5] = ["source", "captured", "kind", "selector", "query_present"];

fn is_extra_key(key: &str) -> bool {
    !key.is_empty()
        && !FIXED_KEYS.contains(&key)
        && key
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// The `source` a clipping may record for `kind`: a URL loses its userinfo,
/// query and fragment (and the flag says a query or fragment was there); a
/// path is kept as is. Works on unparsable URLs too, since Raven's display
/// URL may still hold `{{placeholders}}`.
pub fn public_source(kind: Kind, source: &str) -> (String, bool) {
    if !kind.source_is_url() {
        return (source.to_string(), false);
    }
    let without_query = source.split(['?', '#']).next().unwrap_or_default();
    let query_present = without_query.len() < source.len();
    let public = match without_query.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = match rest.split_once('/') {
                Some((authority, path)) => (authority, Some(path)),
                None => (rest, None),
            };
            let host = authority.rsplit('@').next().unwrap_or(authority);
            match path {
                Some(path) => format!("{scheme}://{host}/{path}"),
                None => format!("{scheme}://{host}"),
            }
        }
        None => without_query.to_string(),
    };
    (public, query_present)
}

/// A Markdown fence longer than any backtick run inside `text`, so embedded
/// content can never close the block early.
pub fn fence_for(text: &str) -> String {
    let longest_run = text
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    "`".repeat(longest_run.max(2) + 1)
}

impl Clipping {
    pub fn parse(text: &str) -> Result<Self> {
        let rest = text
            .strip_prefix("---\n")
            .context("clipping does not start with a front matter block")?;
        let (front, body) = rest
            .split_once("\n---\n")
            .or_else(|| rest.strip_suffix("\n---").map(|front| (front, "")))
            .context("clipping front matter is not closed")?;
        let mut source = None;
        let mut captured = String::new();
        let mut kind = None;
        let mut selector = None;
        let mut query_present = false;
        let mut extra = Vec::new();
        for line in front.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "source" => source = Some(value.to_string()),
                "captured" => captured = value.to_string(),
                "kind" => {
                    kind = Some(
                        Kind::parse(value).ok_or_else(|| anyhow!("unknown clipping kind {value:?}"))?,
                    )
                }
                "selector" => selector = Some(value.to_string()).filter(|text| !text.is_empty()),
                "query_present" => query_present = value == "true",
                key if is_extra_key(key) => extra.push((key.to_string(), value.to_string())),
                _ => {}
            }
        }
        Ok(Self {
            source: source.context("clipping front matter has no source")?,
            captured,
            kind: kind.context("clipping front matter has no kind")?,
            selector,
            query_present,
            extra,
            body: body.to_string(),
        })
    }

    pub fn serialize(&self) -> String {
        let mut out = String::from("---\n");
        out.push_str(&format!("source: {}\n", single_line(&self.source)));
        out.push_str(&format!("captured: {}\n", single_line(&self.captured)));
        out.push_str(&format!("kind: {}\n", self.kind.as_str()));
        if let Some(selector) = self.selector.as_deref().filter(|text| !text.is_empty()) {
            out.push_str(&format!("selector: {}\n", single_line(selector)));
        }
        if self.query_present {
            out.push_str("query_present: true\n");
        }
        for (key, value) in &self.extra {
            if is_extra_key(key) {
                out.push_str(&format!("{key}: {}\n", single_line(value)));
            }
        }
        out.push_str("---\n");
        out.push_str(&self.body);
        out
    }
}

/// Front matter is line based, so a value must never contain a line break.
fn single_line(text: &str) -> String {
    text.chars()
        .map(|character| if character == '\n' || character == '\r' { ' ' } else { character })
        .collect::<String>()
        .trim()
        .to_string()
}

pub fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for character in title.trim().chars() {
        if character.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(character.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
        if slug.len() >= SLUG_MAX_LEN {
            break;
        }
    }
    let slug = slug.trim_end_matches('-').to_string();
    if slug.is_empty() { "clipping".into() } else { slug }
}

/// The slug for a capture: the page title or selector when there is one,
/// else the URL without its scheme, else the file's stem.
pub fn slug_for(kind: Kind, source: &str, title: Option<&str>) -> String {
    if let Some(title) = title.map(str::trim).filter(|title| !title.is_empty()) {
        return slugify(title);
    }
    match kind {
        Kind::Pdf => {
            let stem = std::path::Path::new(source)
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            slugify(&stem)
        }
        Kind::Pick | Kind::Fetch | Kind::Raven => {
            let (public, _) = public_source(kind, source);
            let without_scheme = public
                .split_once("://")
                .map_or(public.as_str(), |(_, rest)| rest);
            slugify(without_scheme)
        }
    }
}

pub fn stem(date: &str, slug: &str) -> String {
    format!("{date}-{slug}")
}

pub fn numbered_stem(stem: &str, number: usize) -> String {
    format!("{stem}-{number}")
}

pub fn file_name(stem: &str) -> String {
    format!("{stem}.md")
}

pub fn asset_file_name(stem: &str, extension: &str) -> String {
    format!("{stem}.{extension}")
}

/// `YYYY-MM-DD-<slug>.md` where the slug is lowercase ascii letters, digits
/// or dashes, non-empty and bounded. Anything else is not ours to read.
pub fn is_valid_clipping_name(name: &str) -> bool {
    split_clipping_name(name).is_some()
}

/// Splits `YYYY-MM-DD-<slug>.md` into `(date, slug)`, or `None` for any
/// other shape. Works on bytes and only slices after proving every byte is
/// ASCII, so a multibyte or hostile name can never panic here.
pub fn split_clipping_name(name: &str) -> Option<(&str, &str)> {
    let stem = name.strip_suffix(".md")?;
    if !stem.is_ascii()
        || stem.len() <= DATE_PREFIX_LEN
        || stem.len() > DATE_PREFIX_LEN + SLUG_MAX_LEN + 8
    {
        return None;
    }
    let date = stem.get(..DATE_PREFIX_LEN - 1)?;
    let separator = stem.as_bytes().get(DATE_PREFIX_LEN - 1)?;
    let slug = stem.get(DATE_PREFIX_LEN..)?;
    let date_shape = date.len() == "0000-00-00".len()
        && date
            .bytes()
            .zip("0000-00-00".bytes())
            .all(|(actual, expected)| match expected {
                b'0' => actual.is_ascii_digit(),
                other => actual == other,
            });
    let slug_shape = !slug.is_empty()
        && !slug.starts_with('-')
        && !slug.ends_with('-')
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    (date_shape && *separator == b'-' && slug_shape).then_some((date, slug))
}

/// True for a `.hulder`-relative path inside `clippings/`.
pub fn is_clipping_relative_path(relative: &str) -> bool {
    relative
        .strip_prefix(DIR)
        .is_some_and(|rest| rest.starts_with('/') && rest.len() > 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_round_trips_front_matter() {
        let text = "---\nsource: https://example.com/a?b=c\ncaptured: 2026-10-13T09:30:00Z\nkind: pick\nselector: #main > p.lead\n---\n# Heading\n\nBody.\n";
        let clipping = Clipping::parse(text).expect("parse");
        assert_eq!(clipping.source, "https://example.com/a?b=c");
        assert_eq!(clipping.captured, "2026-10-13T09:30:00Z");
        assert_eq!(clipping.kind, Kind::Pick);
        assert_eq!(clipping.selector.as_deref(), Some("#main > p.lead"));
        assert_eq!(clipping.body, "# Heading\n\nBody.\n");
        assert_eq!(clipping.serialize(), text);
    }

    #[test]
    fn selector_is_omitted_when_absent() {
        let clipping = Clipping {
            source: "/Users/me/doc.pdf".into(),
            captured: "2026-10-13T09:30:00Z".into(),
            kind: Kind::Pdf,
            selector: None,
            query_present: false,
            extra: Vec::new(),
            body: "text".into(),
        };
        let text = clipping.serialize();
        assert_eq!(
            text,
            "---\nsource: /Users/me/doc.pdf\ncaptured: 2026-10-13T09:30:00Z\nkind: pdf\n---\ntext"
        );
        assert_eq!(Clipping::parse(&text).expect("parse"), clipping);
    }

    #[test]
    fn parse_rejects_bad_input() {
        assert!(Clipping::parse("no front matter").is_err());
        assert!(Clipping::parse("---\nsource: x\nkind: nope\n---\n").is_err());
        assert!(Clipping::parse("---\nkind: pick\n---\n").is_err());
        assert!(Clipping::parse("---\nsource: x\nkind: pick\n").is_err());
    }

    #[test]
    fn kinds_parse_and_print() {
        for kind in Kind::ALL {
            assert_eq!(Kind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(Kind::parse("fetch"), Some(Kind::Fetch));
        assert_eq!(Kind::parse("web"), None);
    }

    #[test]
    fn url_sources_lose_userinfo_query_and_fragment() {
        assert_eq!(
            public_source(
                Kind::Fetch,
                "https://alice:hunter2@api.test/reset?token=abc123#frag"
            ),
            ("https://api.test/reset".to_string(), true)
        );
        assert_eq!(
            public_source(Kind::Pick, "https://example.com/a/b"),
            ("https://example.com/a/b".to_string(), false)
        );
        assert_eq!(
            public_source(Kind::Raven, "https://u:p@h.test"),
            ("https://h.test".to_string(), false)
        );
        assert_eq!(
            public_source(Kind::Fetch, "{{base}}/x?k=v"),
            ("{{base}}/x".to_string(), true)
        );
        assert_eq!(
            public_source(Kind::Pdf, "/Users/me/a?b.pdf"),
            ("/Users/me/a?b.pdf".to_string(), false)
        );
    }

    #[test]
    fn url_slugs_come_from_the_host_and_path_only() {
        assert_eq!(
            slug_for(
                Kind::Fetch,
                "https://alice:hunter2@example.com/reset?token=abc123#x",
                None
            ),
            "example-com-reset"
        );
        assert_eq!(
            slug_for(Kind::Pick, "https://example.com/?reset-token=abc123", None),
            "example-com"
        );
    }

    #[test]
    fn extra_front_matter_and_query_flag_round_trip() {
        let clipping = Clipping {
            source: "https://api.test/users".into(),
            captured: "2026-10-13T10:00:00Z".into(),
            kind: Kind::Raven,
            selector: None,
            query_present: true,
            extra: vec![("request".into(), "list users".into()), ("status".into(), "200".into())],
            body: "body".into(),
        };
        let text = clipping.serialize();
        assert_eq!(
            text,
            "---\nsource: https://api.test/users\ncaptured: 2026-10-13T10:00:00Z\nkind: raven\nquery_present: true\nrequest: list users\nstatus: 200\n---\nbody"
        );
        assert_eq!(Clipping::parse(&text).expect("parse"), clipping);
        let plain = Clipping::parse("---\nsource: x\nkind: pdf\n---\n").expect("parse");
        assert!(!plain.query_present);
        assert!(plain.extra.is_empty());
        let bad_key = Clipping {
            extra: vec![("bad key".into(), "v".into()), ("source".into(), "v".into())],
            ..clipping
        };
        assert!(!bad_key.serialize().contains("bad key"));
        assert_eq!(bad_key.serialize().matches("source:").count(), 1);
    }

    #[test]
    fn fence_outgrows_any_backtick_run_in_the_content() {
        assert_eq!(fence_for("plain"), "```");
        assert_eq!(fence_for("a ``` b"), "````");
        assert_eq!(fence_for("a ````` b"), "``````");
        assert_eq!(fence_for("`` short"), "```");
    }

    #[test]
    fn slug_and_file_names() {
        assert_eq!(slugify("  Fix the Thing!! (v2) "), "fix-the-thing-v2");
        assert_eq!(slugify("???"), "clipping");
        assert!(slugify(&"a".repeat(100)).len() <= SLUG_MAX_LEN);
        assert_eq!(stem("2026-10-13", "fix-it"), "2026-10-13-fix-it");
        assert_eq!(file_name("2026-10-13-fix-it"), "2026-10-13-fix-it.md");
        assert_eq!(
            asset_file_name("2026-10-13-fix-it", "png"),
            "2026-10-13-fix-it.png"
        );
        assert_eq!(numbered_stem("2026-10-13-fix-it", 2), "2026-10-13-fix-it-2");
    }

    #[test]
    fn source_slugs_prefer_the_title_then_the_url_then_the_file() {
        assert_eq!(
            slug_for(Kind::Fetch, "https://docs.rs/gpui", Some("GPUI docs")),
            "gpui-docs"
        );
        assert_eq!(
            slug_for(Kind::Fetch, "https://docs.rs/gpui/latest/", None),
            "docs-rs-gpui-latest"
        );
        assert_eq!(
            slug_for(Kind::Pdf, "/Users/me/Reports/Q3 Report.pdf", None),
            "q3-report"
        );
        assert_eq!(
            slug_for(Kind::Pick, "https://example.com/", Some("#main > p.lead")),
            "main-p-lead"
        );
        assert_eq!(slug_for(Kind::Pick, "", None), "clipping");
    }

    #[test]
    fn clipping_names_are_dated_lowercase_markdown() {
        for name in [
            "2026-10-13-fix-it.md",
            "2026-10-13-fix-it-2.md",
            "2026-01-01-a.md",
        ] {
            assert!(is_valid_clipping_name(name), "{name}");
        }
        for name in [
            "",
            "fix-it.md",
            "2026-10-13-.md",
            "2026-10-13-Fix.md",
            "2026-10-13-fix.txt",
            "2026-10-13-fix.md/",
            "../2026-10-13-fix.md",
            "2026-10-13-fix/../x.md",
            "2026-10-13-fi\\x.md",
            "2026-10-13-fix it.md",
            ".2026-10-13-fix.md",
            "2026-10-13-fix.md\0",
        ] {
            assert!(!is_valid_clipping_name(name), "{name:?}");
        }
        let long = format!("2026-10-13-{}.md", "a".repeat(SLUG_MAX_LEN + 10));
        assert!(!is_valid_clipping_name(&long));
    }

    #[test]
    fn hostile_names_never_panic() {
        let mut names: Vec<String> = vec![
            "".into(),
            ".md".into(),
            "-.md".into(),
            "2026-10-13-.md".into(),
            "2026-10-13".into(),
            "2026-10-13-".into(),
            "ä.md".into(),
            "ääääää.md".into(),
            "2026-10-13-ä.md".into(),
            "2026-10-1ä-a.md".into(),
            "\u{1F525}\u{1F525}\u{1F525}\u{1F525}.md".into(),
            "a".repeat(300) + ".md",
            "2026-10-13-".to_string() + &"a".repeat(300) + ".md",
            "\0".into(),
        ];
        let base = "2026-10-13-slug.md";
        for boundary in 0..=base.len() {
            let mut name = String::new();
            name.push_str(&base[..boundary]);
            name.push('ä');
            name.push_str(&base[boundary..]);
            names.push(name);
        }
        for name in &names {
            assert!(!is_valid_clipping_name(name), "{name:?}");
            assert_eq!(split_clipping_name(name), None, "{name:?}");
            assert!(Clipping::parse(name).is_err());
            slugify(name);
            slug_for(Kind::Pdf, name, Some(name));
            is_clipping_relative_path(name);
        }
        assert_eq!(
            split_clipping_name("2026-10-13-fix-it-2.md"),
            Some(("2026-10-13", "fix-it-2"))
        );
    }

    #[test]
    fn relative_paths_under_clippings_are_recognised() {
        assert!(is_clipping_relative_path("clippings/2026-10-13-a.md"));
        assert!(is_clipping_relative_path("clippings/assets/2026-10-13-a.png"));
        assert!(!is_clipping_relative_path("clippings"));
        assert!(!is_clipping_relative_path("notes/2026-10-13.md"));
        assert!(!is_clipping_relative_path("clippingsx/a.md"));
    }
}
