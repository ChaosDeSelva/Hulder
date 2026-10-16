use anyhow::{Context as _, Result, anyhow, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use collections::HashMap;
use path::rel_path::{RelPath, RelPathBuf};
use std::path::{Path, PathBuf};
use util::paths::PathStyle;

/// Directory under the worktree root that holds gloss files.
pub const GLOSSES_DIR: &str = ".hulder/glosses";
/// How many lines above and below the recorded line a gloss searches for its anchor.
pub const REANCHOR_WINDOW: u32 = 200;
/// Largest line number a gloss may record; anything above is clamped so line arithmetic
/// cannot overflow.
pub const MAX_LINE: u32 = 1 << 30;
/// Most glosses of one file that are shown; the rest are counted but hidden.
pub const MAX_GLOSSES_PER_FILE: usize = 500;
/// Longest body a gloss typed in the prompt may have.
pub const MAX_GLOSS_BODY_BYTES: usize = 8 * 1024;

const SECTION_HEADER: &str = "## Gloss";

/// A 1-based inclusive range of lines in the source file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

impl LineRange {
    pub fn new(start: u32, end: u32) -> Self {
        let start = start.clamp(1, MAX_LINE);
        let end = end.clamp(1, MAX_LINE);
        Self {
            start: start.min(end),
            end: start.max(end),
        }
    }

    pub fn single(line: u32) -> Self {
        Self::new(line, line)
    }

    pub fn len(&self) -> u32 {
        self.end.saturating_sub(self.start).saturating_add(1)
    }

    pub fn is_empty(&self) -> bool {
        false
    }
}

/// One note attached to a file or to a line range of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gloss {
    pub lines: Option<LineRange>,
    pub anchor: String,
    pub created: DateTime<Utc>,
    pub author: Option<String>,
    pub body: String,
}

/// The outcome of looking for a gloss's anchor text in the current file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchoring {
    Anchored(LineRange),
    Detached,
}

impl Gloss {
    pub fn file_level(created: DateTime<Utc>, body: String) -> Self {
        Self {
            lines: None,
            anchor: String::new(),
            created,
            author: None,
            body,
        }
    }

    /// Finds the current position of this gloss in a file described by `line_count`
    /// lines and `line_at` (1-based line number to untrimmed line text).
    ///
    /// Builds a [`ReanchorIndex`] over the whole file, so callers with many glosses
    /// should build one index and call [`ReanchorIndex::reanchor`] instead.
    ///
    /// Returns `None` for file-level glosses, which have no anchor.
    pub fn reanchor(
        &self,
        line_count: u32,
        line_at: impl Fn(u32) -> Option<String>,
    ) -> Option<Anchoring> {
        self.lines?;
        let lines: Vec<String> = (1..=line_count)
            .map(|line| line_at(line).unwrap_or_default())
            .collect();
        let text = lines.join("\n");
        let mut index = ReanchorIndex::new(&text, line_count);
        index.reanchor(self)
    }
}

/// Rows of a file keyed by trimmed line text, so re-anchoring a gloss costs one lookup
/// instead of a scan of `2 * REANCHOR_WINDOW` lines. Building it is linear in the text.
pub struct ReanchorIndex<'a> {
    line_count: u32,
    text: &'a str,
    /// Trimmed line text to ascending 1-based line numbers.
    exact: HashMap<&'a str, Vec<u32>>,
    /// Line text with all whitespace removed; built on the first miss of `exact`.
    stripped: Option<HashMap<String, Vec<u32>>>,
}

impl<'a> ReanchorIndex<'a> {
    /// `text` is the whole file; `line_count` bounds the rows considered so an index built
    /// from a buffer snapshot agrees with the snapshot's own row count.
    pub fn new(text: &'a str, line_count: u32) -> Self {
        let mut exact: HashMap<&'a str, Vec<u32>> = HashMap::default();
        for (line_number, line) in numbered_lines(text, line_count) {
            exact.entry(line.trim()).or_default().push(line_number);
        }
        Self {
            line_count,
            text,
            exact,
            stripped: None,
        }
    }

    /// Returns `None` for file-level glosses, which have no anchor.
    pub fn reanchor(&mut self, gloss: &Gloss) -> Option<Anchoring> {
        let lines = gloss.lines?;
        if self.line_count == 0 {
            return Some(Anchoring::Detached);
        }
        let wanted = gloss.anchor.trim();
        let found = self
            .exact
            .get(wanted)
            .and_then(|rows| nearest_within_window(rows, lines.start))
            .or_else(|| {
                let wanted = strip_whitespace(wanted);
                self.stripped_rows(&wanted)
                    .and_then(|rows| nearest_within_window(rows, lines.start))
            });
        Some(match found {
            Some(start) => {
                let end = start
                    .saturating_add(lines.len().saturating_sub(1))
                    .min(self.line_count);
                Anchoring::Anchored(LineRange::new(start, end))
            }
            None => Anchoring::Detached,
        })
    }

    fn stripped_rows(&mut self, wanted: &str) -> Option<&Vec<u32>> {
        let text = self.text;
        let line_count = self.line_count;
        let stripped = self.stripped.get_or_insert_with(|| {
            let mut stripped: HashMap<String, Vec<u32>> = HashMap::default();
            for (line_number, line) in numbered_lines(text, line_count) {
                stripped
                    .entry(strip_whitespace(line))
                    .or_default()
                    .push(line_number);
            }
            stripped
        });
        stripped.get(wanted)
    }
}

/// `(1-based line number, line)` pairs of `text`, stopping at `line_count`.
fn numbered_lines(text: &str, line_count: u32) -> impl Iterator<Item = (u32, &str)> {
    text.split('\n')
        .enumerate()
        .map_while(move |(row, line)| {
            let line_number = u32::try_from(row).ok()?.checked_add(1)?;
            (line_number <= line_count).then_some((line_number, line))
        })
}

/// The row of `rows` (ascending) closest to `origin` and at most `REANCHOR_WINDOW` away;
/// on a tie the row below `origin` wins, matching a search that probes below first.
fn nearest_within_window(rows: &[u32], origin: u32) -> Option<u32> {
    let origin = origin.max(1);
    let split = rows.partition_point(|row| *row < origin);
    let below = rows.get(split).copied();
    let above = split.checked_sub(1).and_then(|index| rows.get(index).copied());
    let distance = |row: u32| row.abs_diff(origin);
    let nearest = match (below, above) {
        (Some(below), Some(above)) if distance(above) < distance(below) => above,
        (Some(below), _) => below,
        (None, Some(above)) => above,
        (None, None) => return None,
    };
    (distance(nearest) <= REANCHOR_WINDOW).then_some(nearest)
}

fn strip_whitespace(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// One section of a gloss file that could not be parsed; the rest of the file still loads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlossParseError {
    /// 1-based index of the `## Gloss` section in the file.
    pub section: usize,
    pub message: String,
}

impl std::fmt::Display for GlossParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "section {}: {}", self.section, self.message)
    }
}

/// The glosses of one file plus the sections that failed to parse.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedGlossFile {
    pub glosses: Vec<Gloss>,
    pub errors: Vec<GlossParseError>,
}

/// Parses the contents of a `.hulder/glosses/<path>.md` file. A section that cannot be
/// parsed is reported in `errors` and skipped, so one bad section never hides the rest.
pub fn parse_gloss_file(text: &str) -> ParsedGlossFile {
    let mut parsed = ParsedGlossFile::default();
    let mut section: Option<Vec<&str>> = None;
    let finish = |lines: Vec<&str>, parsed: &mut ParsedGlossFile| {
        let section_number = parsed.glosses.len() + parsed.errors.len() + 1;
        match parse_section(&lines) {
            Ok(gloss) => parsed.glosses.push(gloss),
            Err(error) => parsed.errors.push(GlossParseError {
                section: section_number,
                message: format!("{error:#}"),
            }),
        }
    };
    for line in text.lines() {
        if line.trim_end() == SECTION_HEADER {
            if let Some(lines) = section.take() {
                finish(lines, &mut parsed);
            }
            section = Some(Vec::new());
        } else if let Some(lines) = section.as_mut() {
            lines.push(line);
        }
    }
    if let Some(lines) = section.take() {
        finish(lines, &mut parsed);
    }
    parsed
}

/// A body line that would read as a section header is written with a leading backslash,
/// and one that already starts with backslashes before the header gets one more.
fn escape_body_line(line: &str) -> String {
    if line.trim_start_matches('\\').trim_end() == SECTION_HEADER {
        format!("\\{line}")
    } else {
        line.to_string()
    }
}

fn unescape_body_line(line: &str) -> &str {
    match line.strip_prefix('\\') {
        Some(rest) if rest.trim_start_matches('\\').trim_end() == SECTION_HEADER => rest,
        _ => line,
    }
}

fn parse_section(lines: &[&str]) -> Result<Gloss> {
    let mut range = None;
    let mut anchor = None;
    let mut created = None;
    let mut author = None;
    let mut body_start = lines.len();
    for (index, line) in lines.iter().enumerate() {
        let Some((key, value)) = line.split_once(':') else {
            body_start = index;
            break;
        };
        let value = value.trim();
        match key.trim() {
            "line" => range = Some(parse_line_range(value)?),
            "anchor" => anchor = Some(value.to_string()),
            "created" => {
                created = Some(
                    DateTime::parse_from_rfc3339(value)
                        .with_context(|| format!("invalid created timestamp {value:?}"))?
                        .with_timezone(&Utc),
                )
            }
            "author" => author = Some(value.to_string()),
            _ => {
                body_start = index;
                break;
            }
        }
    }
    let created = created.ok_or_else(|| anyhow!("gloss section is missing `created:`"))?;
    let body_lines = lines.get(body_start..).unwrap_or_default();
    let trimmed_start = body_lines
        .iter()
        .position(|line| !line.trim().is_empty())
        .unwrap_or(body_lines.len());
    let trimmed_end = body_lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map_or(trimmed_start, |end| end + 1);
    let body = body_lines
        .get(trimmed_start..trimmed_end)
        .unwrap_or_default()
        .iter()
        .map(|line| unescape_body_line(line))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Gloss {
        lines: range,
        anchor: anchor.unwrap_or_default(),
        created,
        author: author.filter(|author| !author.is_empty()),
        body,
    })
}

fn parse_line_range(value: &str) -> Result<LineRange> {
    let (start, end) = match value.split_once('-') {
        Some((start, end)) => (start.trim(), end.trim()),
        None => (value, value),
    };
    let start: u32 = start
        .parse()
        .with_context(|| format!("invalid line range {value:?}"))?;
    let end: u32 = end
        .parse()
        .with_context(|| format!("invalid line range {value:?}"))?;
    if start == 0 || end == 0 {
        bail!("line numbers are 1-based, got {value:?}");
    }
    Ok(LineRange::new(start, end))
}

/// Renders glosses in the canonical on-disk form.
pub fn serialize_gloss_file(glosses: &[Gloss]) -> String {
    let mut output = String::new();
    for gloss in glosses {
        output.push_str(SECTION_HEADER);
        output.push('\n');
        if let Some(lines) = gloss.lines {
            output.push_str(&format!("line: {}-{}\n", lines.start, lines.end));
        }
        output.push_str(&format!("anchor: {}\n", gloss.anchor.trim()));
        output.push_str(&format!("created: {}\n", format_timestamp(gloss.created)));
        if let Some(author) = gloss.author.as_deref().filter(|author| !author.is_empty()) {
            output.push_str(&format!("author: {author}\n"));
        }
        output.push('\n');
        let body = gloss.body.trim();
        if !body.is_empty() {
            for line in body.split('\n') {
                output.push_str(&escape_body_line(line));
                output.push('\n');
            }
        }
        output.push('\n');
    }
    output
}

pub fn format_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Returns the 0-based row of the `## Gloss` header for `gloss` inside a serialized
/// gloss file, matched by its `created` timestamp.
pub fn section_line(text: &str, gloss: &Gloss) -> Option<u32> {
    let wanted = format_timestamp(gloss.created);
    let mut header_row = None;
    for (row, line) in text.lines().enumerate() {
        if line.trim_end() == SECTION_HEADER {
            header_row = Some(row);
        } else if let Some((key, value)) = line.split_once(':')
            && key.trim() == "created"
            && value.trim() == wanted
        {
            return header_row.and_then(|row| u32::try_from(row).ok());
        }
    }
    None
}

/// Normalizes a worktree-relative path, rejecting absolute paths and any `..` component.
pub fn normalize_relative_path(input: &str) -> Result<RelPathBuf> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("empty path");
    }
    if Path::new(trimmed).is_absolute() || trimmed.starts_with(['/', '\\']) {
        bail!("absolute path not allowed: {trimmed:?}");
    }
    if trimmed
        .split(['/', '\\'])
        .any(|component| component == "..")
    {
        bail!("parent directory references not allowed: {trimmed:?}");
    }
    let normalized = RelPath::new(Path::new(trimmed), PathStyle::Unix)?;
    if normalized.is_empty() {
        bail!("empty path");
    }
    Ok(normalized.to_rel_path_buf())
}

/// Worktree-relative path of the gloss file for a source file.
pub fn gloss_file_rel_path(source: &RelPath) -> Result<RelPathBuf> {
    let normalized = normalize_relative_path(source.as_unix_str())?;
    let gloss_file = format!(
        "{GLOSSES_DIR}/{}.md",
        normalized.as_rel_path().as_unix_str()
    );
    let gloss_file = RelPath::new(Path::new(&gloss_file), PathStyle::Unix)?;
    Ok(gloss_file.to_rel_path_buf())
}

/// Absolute path of the gloss file for `source` under worktree `root`. The result is
/// always inside `<root>/.hulder/glosses`.
pub fn gloss_file_path(root: &Path, source: &RelPath) -> Result<PathBuf> {
    let gloss_file = gloss_file_rel_path(source)?;
    Ok(root.join(gloss_file.as_rel_path().as_std_path()))
}

/// Inverse of [`gloss_file_rel_path`]: the source path a gloss file describes, or `None`
/// when the path is not a gloss file.
pub fn source_path_for_gloss_file(gloss_file: &RelPath) -> Option<RelPathBuf> {
    let glosses_dir = RelPath::from_unix_str(GLOSSES_DIR).ok()?;
    let rest = gloss_file.strip_prefix(glosses_dir).ok()?;
    let source = rest.as_unix_str().strip_suffix(".md")?;
    normalize_relative_path(source).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn timestamp(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 13, hour, 30, 0)
            .single()
            .unwrap()
    }

    const SAMPLE: &str = "## Gloss\nanchor: \ncreated: 2026-10-13T09:30:00Z\n\nThis file is the entry point.\n\n## Gloss\nline: 42-48\nanchor: let total = items.iter().sum();\ncreated: 2026-10-13T10:30:00Z\nauthor: claude\n\nSums *all* items.\n\nSecond paragraph.\n\n";

    fn sample_glosses() -> Vec<Gloss> {
        vec![
            Gloss::file_level(timestamp(9), "This file is the entry point.".into()),
            Gloss {
                lines: Some(LineRange::new(42, 48)),
                anchor: "let total = items.iter().sum();".into(),
                created: timestamp(10),
                author: Some("claude".into()),
                body: "Sums *all* items.\n\nSecond paragraph.".into(),
            },
        ]
    }

    #[test]
    fn parses_file_level_and_ranged_glosses() {
        let parsed = parse_gloss_file(SAMPLE);
        assert_eq!(parsed.glosses, sample_glosses());
        assert!(parsed.errors.is_empty());
    }

    #[test]
    fn serializes_canonical_form_and_round_trips() {
        let text = serialize_gloss_file(&sample_glosses());
        assert_eq!(text, SAMPLE);
        assert_eq!(parse_gloss_file(&text).glosses, sample_glosses());
    }

    #[test]
    fn single_line_range_and_unknown_header_lines() {
        let text = "## Gloss\nline: 7\nanchor: fn main() {\ncreated: 2026-10-13T09:30:00Z\nnote: this is body\n";
        let parsed = parse_gloss_file(text).glosses;
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].lines, Some(LineRange::single(7)));
        assert_eq!(parsed[0].body, "note: this is body");
        assert_eq!(parsed[0].author, None);
    }

    #[test]
    fn missing_created_is_an_error() {
        let parsed = parse_gloss_file("## Gloss\nline: 1\nanchor: x\n\nbody\n");
        assert!(parsed.glosses.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(parsed.errors[0].section, 1);
        assert!(parsed.errors[0].message.contains("created"), "{:?}", parsed.errors);
    }

    #[test]
    fn zero_line_is_an_error_and_empty_file_is_empty() {
        let parsed = parse_gloss_file("## Gloss\nline: 0\ncreated: 2026-10-13T09:30:00Z\n");
        assert!(parsed.glosses.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(parse_gloss_file(""), ParsedGlossFile::default());
        assert_eq!(
            parse_gloss_file("# Not a gloss\n\ntext\n"),
            ParsedGlossFile::default()
        );
    }

    #[test]
    fn one_bad_section_does_not_hide_the_others() {
        let text = format!(
            "{SAMPLE}## Gloss\nline: zero\ncreated: 2026-10-13T12:00:00Z\n\nbroken\n\n## Gloss\nanchor: \ncreated: 2026-10-13T13:00:00Z\n\nlast\n\n"
        );
        let parsed = parse_gloss_file(&text);
        assert_eq!(parsed.glosses.len(), 3, "{:?}", parsed.errors);
        assert_eq!(parsed.glosses[2].body, "last");
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(parsed.errors[0].section, 3);
        assert!(parsed.errors[0].message.contains("line range"));
        assert!(parsed.errors[0].to_string().starts_with("section 3:"));
    }

    #[test]
    fn body_line_equal_to_the_section_header_round_trips() {
        let mut gloss = Gloss::file_level(timestamp(9), "before\n## Gloss\nafter".into());
        let text = serialize_gloss_file(std::slice::from_ref(&gloss));
        assert_eq!(text.matches("\n## Gloss\n").count(), 0, "{text}");
        assert!(text.contains("\n\\## Gloss\n"), "{text}");
        let parsed = parse_gloss_file(&text);
        assert_eq!(parsed.glosses.len(), 1);
        assert_eq!(parsed.glosses[0].body, gloss.body);

        gloss.body = "\\## Gloss".into();
        let text = serialize_gloss_file(std::slice::from_ref(&gloss));
        assert!(text.contains("\n\\\\## Gloss\n"), "{text}");
        assert_eq!(parse_gloss_file(&text).glosses[0].body, gloss.body);

        let other = Gloss::file_level(timestamp(9), "\\## not a header".into());
        let text = serialize_gloss_file(std::slice::from_ref(&other));
        assert_eq!(parse_gloss_file(&text).glosses[0].body, other.body);
    }

    #[test]
    fn huge_line_numbers_are_clamped_and_never_overflow() {
        let text = "## Gloss\nline: 4294967295-4294967295\nanchor: needle\ncreated: 2026-10-13T09:30:00Z\n\nbody\n";
        let parsed = parse_gloss_file(text);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        let gloss = &parsed.glosses[0];
        assert_eq!(gloss.lines, Some(LineRange::new(MAX_LINE, MAX_LINE)));
        assert_eq!(gloss.lines.map(|lines| lines.len()), Some(1));
        let lines = file(&["a", "needle"]);
        assert_eq!(reanchor(gloss, &lines), Some(Anchoring::Detached));
        let wide = LineRange::new(1, u32::MAX);
        assert_eq!(wide.len(), MAX_LINE);
        let near = Gloss {
            lines: Some(LineRange::new(1, u32::MAX)),
            ..gloss.clone()
        };
        assert_eq!(
            reanchor(&near, &lines),
            Some(Anchoring::Anchored(LineRange::new(2, 2)))
        );
    }

    #[test]
    fn index_reanchors_many_glosses_with_one_pass() {
        let text: String = (0..20_000)
            .map(|index| format!("line {}\n", index % 7))
            .collect();
        let mut index = ReanchorIndex::new(&text, 20_000);
        for origin in (1..=20_000).step_by(997) {
            let gloss = ranged(origin, origin, &format!("line {}", (origin - 1) % 7));
            assert_eq!(
                index.reanchor(&gloss),
                Some(Anchoring::Anchored(LineRange::new(origin, origin))),
                "origin {origin}"
            );
        }
        let far = ranged(1, 1, "line 3");
        assert_eq!(
            index.reanchor(&far),
            Some(Anchoring::Anchored(LineRange::new(4, 4)))
        );
        let gone = ranged(5, 5, "nowhere");
        assert_eq!(index.reanchor(&gone), Some(Anchoring::Detached));
        let spaced = ranged(2, 2, "line  1");
        assert_eq!(
            index.reanchor(&spaced),
            Some(Anchoring::Anchored(LineRange::new(2, 2)))
        );
    }

    fn file(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    fn reanchor(gloss: &Gloss, lines: &[String]) -> Option<Anchoring> {
        gloss.reanchor(lines.len() as u32, |line| {
            lines.get(line.checked_sub(1)? as usize).cloned()
        })
    }

    fn ranged(start: u32, end: u32, anchor: &str) -> Gloss {
        Gloss {
            lines: Some(LineRange::new(start, end)),
            anchor: anchor.into(),
            created: timestamp(9),
            author: None,
            body: "note".into(),
        }
    }

    #[test]
    fn reanchors_in_place_when_unchanged() {
        let lines = file(&["a", "  let x = 1;", "b"]);
        let gloss = ranged(2, 2, "let x = 1;");
        assert_eq!(
            reanchor(&gloss, &lines),
            Some(Anchoring::Anchored(LineRange::new(2, 2)))
        );
    }

    #[test]
    fn reanchors_after_lines_inserted_above_and_keeps_length() {
        let lines = file(&["new", "new", "a", "let x = 1;", "let y = 2;", "b"]);
        let gloss = ranged(2, 3, "let x = 1;");
        assert_eq!(
            reanchor(&gloss, &lines),
            Some(Anchoring::Anchored(LineRange::new(4, 5)))
        );
    }

    #[test]
    fn nearest_match_wins_over_first_in_file() {
        let lines = file(&[
            "fn f() {}",
            "x",
            "x",
            "x",
            "x",
            "fn f() {}",
            "x",
            "x",
            "fn f() {}",
        ]);
        let gloss = ranged(7, 7, "fn f() {}");
        assert_eq!(
            reanchor(&gloss, &lines),
            Some(Anchoring::Anchored(LineRange::new(6, 6)))
        );
    }

    #[test]
    fn exact_match_preferred_over_whitespace_insensitive_match() {
        let lines = file(&["let x=1;", "y", "let x = 1;"]);
        let gloss = ranged(1, 1, "let x = 1;");
        assert_eq!(
            reanchor(&gloss, &lines),
            Some(Anchoring::Anchored(LineRange::new(3, 3)))
        );
    }

    #[test]
    fn whitespace_insensitive_fallback() {
        let lines = file(&["y", "let  x =1 ;", "z"]);
        let gloss = ranged(2, 2, "let x = 1;");
        assert_eq!(
            reanchor(&gloss, &lines),
            Some(Anchoring::Anchored(LineRange::new(2, 2)))
        );
    }

    #[test]
    fn detached_when_missing_or_outside_window() {
        let lines = file(&["a", "b", "c"]);
        assert_eq!(
            reanchor(&ranged(2, 2, "gone"), &lines),
            Some(Anchoring::Detached)
        );

        let mut far: Vec<String> = (0..500).map(|index| format!("line {index}")).collect();
        far.push("needle".into());
        let gloss = ranged(1, 1, "needle");
        assert_eq!(reanchor(&gloss, &far), Some(Anchoring::Detached));
        let close = ranged(400, 400, "needle");
        assert_eq!(
            reanchor(&close, &far),
            Some(Anchoring::Anchored(LineRange::new(501, 501)))
        );
    }

    #[test]
    fn range_is_clamped_to_file_and_empty_file_detaches() {
        let lines = file(&["a", "needle"]);
        let gloss = ranged(2, 5, "needle");
        assert_eq!(
            reanchor(&gloss, &lines),
            Some(Anchoring::Anchored(LineRange::new(2, 2)))
        );
        assert_eq!(reanchor(&gloss, &[]), Some(Anchoring::Detached));
    }

    #[test]
    fn file_level_gloss_has_no_anchoring() {
        let gloss = Gloss::file_level(timestamp(9), "note".into());
        assert_eq!(reanchor(&gloss, &file(&["a"])), None);
    }

    #[test]
    fn rejects_unsafe_paths() {
        assert!(normalize_relative_path("../etc/passwd").is_err());
        assert!(normalize_relative_path("a/../../b").is_err());
        assert!(normalize_relative_path("a/..").is_err());
        assert!(normalize_relative_path("/etc/passwd").is_err());
        assert!(normalize_relative_path("").is_err());
        assert!(normalize_relative_path("   ").is_err());
        assert!(normalize_relative_path(".").is_err());
    }

    #[test]
    fn normalizes_and_builds_gloss_paths() {
        let normalized = normalize_relative_path("./src/./lib.rs").unwrap();
        assert_eq!(normalized.as_rel_path().as_unix_str(), "src/lib.rs");

        let root = Path::new("/project");
        let path = gloss_file_path(root, normalized.as_rel_path()).unwrap();
        assert_eq!(
            path,
            PathBuf::from("/project/.hulder/glosses/src/lib.rs.md")
        );
        assert!(path.starts_with("/project/.hulder/glosses"));

        let gloss_file = gloss_file_rel_path(normalized.as_rel_path()).unwrap();
        let source = source_path_for_gloss_file(gloss_file.as_rel_path()).unwrap();
        assert_eq!(source.as_rel_path().as_unix_str(), "src/lib.rs");

        let other = RelPath::from_unix_str("src/lib.rs").unwrap();
        assert_eq!(source_path_for_gloss_file(other), None);
        let not_markdown = RelPath::from_unix_str(".hulder/glosses/src/lib.rs").unwrap();
        assert_eq!(source_path_for_gloss_file(not_markdown), None);
    }

    #[test]
    fn section_line_finds_header_row_by_created() {
        let glosses = sample_glosses();
        let text = serialize_gloss_file(&glosses);
        assert_eq!(section_line(&text, &glosses[0]), Some(0));
        assert_eq!(section_line(&text, &glosses[1]), Some(6));
        let missing = Gloss::file_level(timestamp(23), "x".into());
        assert_eq!(section_line(&text, &missing), None);
    }
}
