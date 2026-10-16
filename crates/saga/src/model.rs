//! Pure parsing and formatting for the Saga files under `.hulder/`: tale
//! front matter, slugs and ids, `TODO.md` verses, lore listing and the
//! generated `AGENTS.md`. No I/O and no gpui so other crates can reuse it.

use anyhow::{Context as _, Result, anyhow, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Status {
    Open,
    Doing,
    Blocked,
    Done,
}

impl Status {
    pub const ALL: [Status; 4] = [Status::Open, Status::Doing, Status::Blocked, Status::Done];

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::Doing => "doing",
            Status::Blocked => "blocked",
            Status::Done => "done",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == text.trim())
    }

    pub fn next(self) -> Self {
        match self {
            Status::Open => Status::Doing,
            Status::Doing => Status::Blocked,
            Status::Blocked => Status::Done,
            Status::Done => Status::Open,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    Low,
    Normal,
    High,
}

impl Priority {
    pub const ALL: [Priority; 3] = [Priority::Low, Priority::Normal, Priority::High];

    pub fn as_str(self) -> &'static str {
        match self {
            Priority::Low => "low",
            Priority::Normal => "normal",
            Priority::High => "high",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|priority| priority.as_str() == text.trim())
    }

    pub fn next(self) -> Self {
        match self {
            Priority::Low => Priority::Normal,
            Priority::Normal => Priority::High,
            Priority::High => Priority::Low,
        }
    }
}

pub const CHRONICLE_HEADING: &str = "## Chronicle";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tale {
    pub id: u32,
    pub title: String,
    pub status: Status,
    pub priority: Priority,
    pub tags: Vec<String>,
    pub created: String,
    pub updated: String,
    /// Front matter lines with keys we do not know, kept so a round trip
    /// never drops what an agent or the user added by hand.
    pub extra: Vec<String>,
    /// Everything after the closing `---`, verbatim.
    pub body: String,
}

impl Tale {
    pub fn new(id: u32, title: &str, today: &str) -> Self {
        Self {
            id,
            title: title.trim().to_string(),
            status: Status::Open,
            priority: Priority::Normal,
            tags: Vec::new(),
            created: today.to_string(),
            updated: today.to_string(),
            extra: Vec::new(),
            body: format!("\n{CHRONICLE_HEADING}\n- {today} created\n"),
        }
    }

    pub fn file_name(id: u32, slug: &str) -> String {
        format!("{id:04}-{slug}.md")
    }

    pub fn parse(text: &str) -> Result<Self> {
        let rest = text
            .strip_prefix("---\n")
            .context("tale does not start with a front matter block")?;
        let (front, body) = rest
            .split_once("\n---\n")
            .or_else(|| rest.strip_suffix("\n---").map(|front| (front, "")))
            .context("tale front matter is not closed")?;

        let mut id = None;
        let mut title = None;
        let mut status = Status::Open;
        let mut priority = Priority::Normal;
        let mut tags = Vec::new();
        let mut created = String::new();
        let mut updated = String::new();
        let mut extra = Vec::new();
        for line in front.lines() {
            let Some((key, value)) = line.split_once(':') else {
                extra.push(line.to_string());
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "id" => {
                    id = Some(
                        value
                            .parse::<u32>()
                            .with_context(|| format!("tale id {value:?} is not a number"))?,
                    )
                }
                "title" => title = Some(value.to_string()),
                "status" => {
                    status = Status::parse(value)
                        .ok_or_else(|| anyhow!("unknown tale status {value:?}"))?
                }
                "priority" => {
                    priority = Priority::parse(value)
                        .ok_or_else(|| anyhow!("unknown tale priority {value:?}"))?
                }
                "tags" => tags = parse_tags(value),
                "created" => created = value.to_string(),
                "updated" => updated = value.to_string(),
                _ => extra.push(line.to_string()),
            }
        }
        Ok(Self {
            id: id.context("tale front matter has no id")?,
            title: title.context("tale front matter has no title")?,
            status,
            priority,
            tags,
            created,
            updated,
            extra,
            body: body.to_string(),
        })
    }

    pub fn serialize(&self) -> String {
        let mut out = String::from("---\n");
        out.push_str(&format!("id: {}\n", self.id));
        out.push_str(&format!("title: {}\n", self.title));
        out.push_str(&format!("status: {}\n", self.status.as_str()));
        out.push_str(&format!("priority: {}\n", self.priority.as_str()));
        out.push_str(&format!("tags: [{}]\n", self.tags.join(", ")));
        out.push_str(&format!("created: {}\n", self.created));
        out.push_str(&format!("updated: {}\n", self.updated));
        for line in &self.extra {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("---\n");
        out.push_str(&self.body);
        out
    }

    pub fn touch(&mut self, today: &str) {
        self.updated = today.to_string();
    }

    /// Refuses front matter values that would break out of their line: a
    /// newline in a title or tag would otherwise serialize as a new key.
    pub fn validate_front_matter(&self) -> Result<()> {
        let fields = [
            ("title", &self.title),
            ("created", &self.created),
            ("updated", &self.updated),
        ];
        for (name, value) in fields {
            if value.chars().any(char::is_control) {
                bail!("tale {name} must not contain control characters");
            }
        }
        if self
            .tags
            .iter()
            .any(|tag| tag.chars().any(char::is_control))
        {
            bail!("tale tags must not contain control characters");
        }
        if self
            .extra
            .iter()
            .any(|line| line.chars().any(char::is_control))
        {
            bail!("tale front matter must not contain control characters");
        }
        Ok(())
    }

    /// Appends a bullet under `## Chronicle`, creating the section when the
    /// body has none.
    pub fn append_chronicle(&mut self, line: &str) {
        if !self.body.ends_with('\n') && !self.body.is_empty() {
            self.body.push('\n');
        }
        if !self
            .body
            .lines()
            .any(|existing| existing.trim_end() == CHRONICLE_HEADING)
        {
            if !self.body.is_empty() {
                self.body.push('\n');
            }
            self.body.push_str(CHRONICLE_HEADING);
            self.body.push('\n');
        }
        self.body.push_str("- ");
        self.body.push_str(line.trim());
        self.body.push('\n');
    }

    /// The text handed to an agent: path first so it can open the file, then
    /// the full tale so it needs no round trip to read it.
    pub fn handoff_text(path: &str, tale_text: &str) -> String {
        format!(
            "{}\n\n{}",
            sanitize_for_terminal(path),
            sanitize_for_terminal(tale_text)
        )
    }
}

pub use herdr::sanitize::{normalize_newlines, sanitize_for_terminal};

/// A name safe to hand to herdr as a workspace label or agent name: only
/// `[A-Za-z0-9._-]`, never empty, never starting with a dot or dash.
pub fn herd_label(text: &str) -> String {
    let mut label: String = sanitize_for_terminal(text)
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
        .take(SLUG_MAX_LEN)
        .collect();
    while label.starts_with(['.', '-']) {
        label.remove(0);
    }
    if label.is_empty() {
        "tale".into()
    } else {
        label
    }
}

fn parse_tags(value: &str) -> Vec<String> {
    value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|tag| tag.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|tag| !tag.is_empty())
        .collect()
}

pub const SLUG_MAX_LEN: usize = 48;

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
    if slug.is_empty() { "tale".into() } else { slug }
}

/// Reads the `NNNN` prefix of each ticket file name; the next id is one past
/// the largest, or 1 when there are none.
pub fn next_id<'a>(file_names: impl IntoIterator<Item = &'a str>) -> u32 {
    file_names
        .into_iter()
        .filter_map(id_from_file_name)
        .max()
        .map_or(1, |max| max + 1)
}

pub fn id_from_file_name(file_name: &str) -> Option<u32> {
    let digits: String = file_name.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || !file_name[digits.len()..].starts_with('-') {
        return None;
    }
    digits.parse().ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verse {
    /// Zero-based line index in `TODO.md`, the handle used to toggle it.
    pub line: usize,
    /// Leading whitespace width, so nested verses render indented.
    pub depth: usize,
    pub done: bool,
    pub text: String,
}

fn split_verse_line(line: &str) -> Option<(usize, bool, &str)> {
    let trimmed = line.trim_start();
    let depth = line.len() - trimmed.len();
    let after_bullet = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))?;
    let (done, text) = if let Some(text) = after_bullet.strip_prefix("[ ] ") {
        (false, text)
    } else if let Some(text) = after_bullet
        .strip_prefix("[x] ")
        .or_else(|| after_bullet.strip_prefix("[X] "))
    {
        (true, text)
    } else if after_bullet == "[ ]" {
        (false, "")
    } else if after_bullet == "[x]" || after_bullet == "[X]" {
        (true, "")
    } else {
        return None;
    };
    Some((depth, done, text))
}

pub fn parse_verses(todo: &str) -> Vec<Verse> {
    todo.lines()
        .enumerate()
        .filter_map(|(line, text)| {
            split_verse_line(text).map(|(depth, done, text)| Verse {
                line,
                depth,
                done,
                text: text.trim_end().to_string(),
            })
        })
        .collect()
}

/// Flips the checkbox on one line, leaving every other byte alone. Returns
/// `None` when that line is not a verse.
pub fn toggle_verse(todo: &str, line_index: usize) -> Option<String> {
    let mut lines: Vec<&str> = todo.split('\n').collect();
    let line = *lines.get(line_index)?;
    let (depth, done, _) = split_verse_line(line)?;
    let prefix_len = depth + 2;
    let toggled = if done {
        format!("{}[ ]{}", &line[..prefix_len], &line[prefix_len + 3..])
    } else {
        format!("{}[x]{}", &line[..prefix_len], &line[prefix_len + 3..])
    };
    lines[line_index] = &toggled;
    Some(lines.join("\n"))
}

pub fn append_verse(todo: &str, text: &str) -> String {
    let mut out = todo.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("- [ ] ");
    out.push_str(text.trim());
    out.push('\n');
    out
}

pub const NOTES_FILE: &str = "NOTES.md";
pub const TODO_FILE: &str = "TODO.md";
pub const AGENTS_FILE: &str = "AGENTS.md";
pub const CONFIG_FILE: &str = "saga.json";
pub const TICKETS_DIR: &str = "tickets";
pub const NOTES_DIR: &str = "notes";
pub const BIN_DIR: &str = "bin";

/// Lore is every Markdown file under `.hulder/` that is not a tale, a
/// clipping or a control file, sorted so `NOTES.md` and other root files
/// come before `notes/`.
pub fn lore_files<'a>(relative_paths: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let mut lore: Vec<&str> = relative_paths
        .into_iter()
        .filter(|path| path.ends_with(".md"))
        .filter(|path| !path.starts_with(&format!("{TICKETS_DIR}/")))
        .filter(|path| !path.starts_with(&format!("{BIN_DIR}/")))
        .filter(|path| !clippings::is_clipping_relative_path(path))
        .filter(|path| *path != TODO_FILE && *path != AGENTS_FILE)
        .collect();
    lore.sort_by_key(|path| (path.contains('/'), *path != NOTES_FILE, *path));
    lore
}

pub fn lore_file_name(date: &str) -> String {
    format!("{NOTES_DIR}/{date}.md")
}

pub const AGENTS_TEMPLATE: &str = "# Saga: project notes, tales and verses

This folder is the project's Saga, kept by Hulder as plain Markdown so you
can read it as context without any integration.

- `NOTES.md` and `notes/YYYY-MM-DD.md` are free-form lore about the project.
- `TODO.md` is a checkbox list of quick verses (`- [ ]` / `- [x]`).
- `tickets/NNNN-slug.md` holds one tale (ticket) per file with YAML-ish front
  matter: `id`, `title`, `status` (open|doing|blocked|done), `priority`
  (low|normal|high), `tags`, `created`, `updated`.
- `bin/` is build output and is never committed.

When you work on a tale, append a dated bullet to its `## Chronicle` section
(for example `- 2026-10-13 implemented the parser, tests green`). Do not edit
the front matter; the user changes status and priority from the Saga panel.
";

pub const NOTES_TEMPLATE: &str = "# Notes\n";
pub const TODO_TEMPLATE: &str = "# Todo\n\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tale_round_trips_front_matter() {
        let text = "---\nid: 12\ntitle: Fix: the thing\nstatus: doing\npriority: high\ntags: [ui, bug]\ncreated: 2026-10-13\nupdated: 2026-10-14\n---\n\nBody line.\n\n## Chronicle\n- 2026-10-13 created\n";
        let tale = Tale::parse(text).expect("parse");
        assert_eq!(tale.id, 12);
        assert_eq!(tale.title, "Fix: the thing");
        assert_eq!(tale.status, Status::Doing);
        assert_eq!(tale.priority, Priority::High);
        assert_eq!(tale.tags, vec!["ui", "bug"]);
        assert_eq!(tale.created, "2026-10-13");
        assert_eq!(tale.updated, "2026-10-14");
        assert_eq!(tale.serialize(), text);
    }

    #[test]
    fn tale_keeps_unknown_keys_and_empty_tags() {
        let text = "---\nid: 1\ntitle: T\nstatus: open\npriority: normal\ntags: []\ncreated: d\nupdated: d\nowner: me\n---\n";
        let tale = Tale::parse(text).expect("parse");
        assert!(tale.tags.is_empty());
        assert_eq!(tale.extra, vec!["owner: me"]);
        assert_eq!(tale.body, "");
        assert_eq!(tale.serialize(), text);
    }

    #[test]
    fn tale_parse_rejects_bad_input() {
        assert!(Tale::parse("no front matter").is_err());
        assert!(Tale::parse("---\ntitle: x\n---\n").is_err());
        assert!(Tale::parse("---\nid: 1\ntitle: x\nstatus: weird\n---\n").is_err());
        assert!(Tale::parse("---\nid: one\ntitle: x\n---\n").is_err());
    }

    #[test]
    fn new_tale_has_chronicle_and_append_extends_it() {
        let mut tale = Tale::new(3, "  Ship it ", "2026-10-13");
        assert_eq!(tale.title, "Ship it");
        assert!(tale.body.contains("## Chronicle\n- 2026-10-13 created\n"));
        tale.append_chronicle("2026-10-14 started by claude");
        assert!(
            tale.body
                .ends_with("- 2026-10-13 created\n- 2026-10-14 started by claude\n")
        );
        let parsed = Tale::parse(&tale.serialize()).expect("round trip");
        assert_eq!(parsed, tale);

        let mut bare = Tale::parse("---\nid: 1\ntitle: x\n---\nText").expect("parse");
        bare.append_chronicle("line");
        assert_eq!(bare.body, "Text\n\n## Chronicle\n- line\n");
    }

    #[test]
    fn front_matter_validation_refuses_control_characters() {
        let mut tale = Tale::new(1, "Plain title", "2026-10-13");
        tale.tags = vec!["ui".into()];
        assert!(tale.validate_front_matter().is_ok());
        tale.title = "a\nstatus: done".into();
        assert!(tale.validate_front_matter().is_err());
        tale.title = "ok".into();
        tale.tags = vec!["a\tb".into()];
        assert!(tale.validate_front_matter().is_err());
        tale.tags.clear();
        tale.extra = vec!["owner: me\rx".into()];
        assert!(tale.validate_front_matter().is_err());
    }

    #[test]
    fn status_and_priority_cycle() {
        assert_eq!(Status::Open.next(), Status::Doing);
        assert_eq!(Status::Doing.next(), Status::Blocked);
        assert_eq!(Status::Blocked.next(), Status::Done);
        assert_eq!(Status::Done.next(), Status::Open);
        assert_eq!(Priority::Low.next(), Priority::Normal);
        assert_eq!(Priority::Normal.next(), Priority::High);
        assert_eq!(Priority::High.next(), Priority::Low);
        assert_eq!(Status::parse("blocked"), Some(Status::Blocked));
        assert_eq!(Priority::parse("nope"), None);
    }

    #[test]
    fn slug_and_next_id() {
        assert_eq!(slugify("  Fix the Thing!! (v2) "), "fix-the-thing-v2");
        assert_eq!(slugify("???"), "tale");
        assert_eq!(slugify("Ünïcode café"), "n-code-caf");
        assert!(slugify(&"a".repeat(100)).len() <= SLUG_MAX_LEN);
        assert_eq!(
            next_id(["0001-a.md", "0007-b.md", "README.md", "12x.md"]),
            8
        );
        assert_eq!(next_id(Vec::<&str>::new()), 1);
        assert_eq!(Tale::file_name(8, "fix-it"), "0008-fix-it.md");
        assert_eq!(id_from_file_name("0008-fix-it.md"), Some(8));
        assert_eq!(id_from_file_name("fix-it.md"), None);
    }

    #[test]
    fn handoff_text_is_path_then_body_with_lf_only() {
        assert_eq!(Tale::handoff_text("/p/t.md", "body"), "/p/t.md\n\nbody");
        assert_eq!(
            Tale::handoff_text("/p/t.md", "a\r\nb\rc"),
            "/p/t.md\n\na\nb\nc"
        );
        assert_eq!(normalize_newlines("x\r\n\r"), "x\n\n");
    }

    #[test]
    fn sanitize_strips_terminal_control_sequences() {
        assert_eq!(
            sanitize_for_terminal("a\u{1B}]0;title\u{07}b"),
            "ab",
            "OSC title sequence ending in BEL"
        );
        assert_eq!(
            sanitize_for_terminal("a\u{1B}]0;title\u{1B}\\b"),
            "ab",
            "OSC ending in ST"
        );
        assert_eq!(
            sanitize_for_terminal("x\u{1B}[31;1mred\u{1B}[0my"),
            "xredy",
            "CSI color sequences"
        );
        assert_eq!(
            sanitize_for_terminal("\u{1B}P1;2|data\u{1B}\\z"),
            "z",
            "DCS"
        );
        assert_eq!(
            sanitize_for_terminal("\u{1B}cq\u{1B}(Br"),
            "qr",
            "two-char escapes"
        );
        assert_eq!(
            sanitize_for_terminal("\u{9B}31mlone c1 csi"),
            "31mlone c1 csi"
        );
        assert_eq!(
            sanitize_for_terminal("line\tone\r\nline\u{00}two\u{7F}\u{08}"),
            "line\tone\nlinetwo",
            "newline and tab survive, other C0/DEL do not"
        );
        assert_eq!(
            sanitize_for_terminal(
                "a\u{202E}evil\u{202C}b\u{200B}c\u{FEFF}d\u{2066}e\u{2069}\u{2028}f"
            ),
            "aevilbcdef",
            "bidi, zero-width, BOM and separators"
        );
        assert_eq!(sanitize_for_terminal("\u{1B}"), "", "trailing ESC");
        assert_eq!(sanitize_for_terminal("\u{1B}[31;"), "", "unterminated CSI");
        assert_eq!(sanitize_for_terminal("plain ünïcode ✓"), "plain ünïcode ✓");
        assert_eq!(
            Tale::handoff_text("/p/\u{1B}[2Jt.md", "\u{1B}]0;x\u{07}body\u{202E}"),
            "/p/t.md\n\nbody"
        );
        assert_eq!(herd_label("\u{1B}[31mfix\u{1B}[0m"), "fix");
    }

    #[test]
    fn herd_label_is_restricted() {
        assert_eq!(herd_label("0001-fix-it"), "0001-fix-it");
        assert_eq!(herd_label("rm -rf /; $(id) `x` a.b_c"), "rm-rfidxa.b_c");
        assert_eq!(herd_label("--..evil"), "evil");
        assert_eq!(herd_label("\u{1F525}\n;|&"), "tale");
        assert!(herd_label(&"a".repeat(200)).len() <= SLUG_MAX_LEN);
        for character in herd_label("any text!").chars() {
            assert!(character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'));
        }
    }

    #[test]
    fn parses_nested_verses_and_toggles_in_place() {
        let text = "# Todo\n\n- [ ] one\n  - [x] nested\n- [X] two\nplain line\n* [ ]\n";
        let verses = parse_verses(text);
        assert_eq!(verses.len(), 4);
        assert_eq!(
            verses[1],
            Verse {
                line: 3,
                depth: 2,
                done: true,
                text: "nested".into()
            }
        );
        assert_eq!(verses[3].text, "");
        let toggled = toggle_verse(text, 2).expect("line 2 is a verse");
        assert_eq!(
            toggled,
            "# Todo\n\n- [x] one\n  - [x] nested\n- [X] two\nplain line\n* [ ]\n"
        );
        let untoggled = toggle_verse(text, 3).expect("line 3 is a verse");
        assert_eq!(
            untoggled,
            "# Todo\n\n- [ ] one\n  - [ ] nested\n- [X] two\nplain line\n* [ ]\n"
        );
        assert!(toggle_verse(text, 5).is_none());
        assert!(toggle_verse(text, 99).is_none());
    }

    #[test]
    fn appends_a_verse_keeping_trailing_newline() {
        assert_eq!(append_verse("", "first"), "- [ ] first\n");
        assert_eq!(append_verse("- [ ] a", "b"), "- [ ] a\n- [ ] b\n");
        assert_eq!(append_verse("- [ ] a\n", " b "), "- [ ] a\n- [ ] b\n");
    }

    #[test]
    fn lore_is_markdown_outside_tickets_and_control_files() {
        let files = [
            "notes/2026-10-13.md",
            "tickets/0001-x.md",
            "ideas.md",
            "TODO.md",
            "AGENTS.md",
            "saga.json",
            "NOTES.md",
            "bin/out.md",
            "clippings/2026-10-13-page.md",
            "clippings/assets/2026-10-13-page.md",
        ];
        assert_eq!(
            lore_files(files),
            vec!["NOTES.md", "ideas.md", "notes/2026-10-13.md"]
        );
        assert_eq!(lore_file_name("2026-10-13"), "notes/2026-10-13.md");
    }

    #[test]
    fn agents_template_names_the_files() {
        for needle in ["NOTES.md", "TODO.md", "tickets/", "## Chronicle", "bin/"] {
            assert!(AGENTS_TEMPLATE.contains(needle), "{needle}");
        }
    }
}
