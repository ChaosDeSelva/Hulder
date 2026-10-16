use anyhow::{Context as _, Result};
use fs::Fs;
use git::status::GitSummary;
use serde::{Deserialize, Serialize};
use std::{
    io::Read as _,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

pub const RECENT_CAP: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitFilter {
    Modified,
    Added,
    Untracked,
    Conflict,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    pub text: String,
    pub ext: Option<String>,
    pub git: Option<GitFilter>,
    pub since: Option<Duration>,
    pub size_gt: Option<u64>,
}

pub struct EntryFacts<'a> {
    pub path: &'a str,
    pub is_dir: bool,
    pub mtime: Option<SystemTime>,
    pub size: u64,
    pub git: GitSummary,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FolderStats {
    pub files: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrovePath {
    pub worktree: PathBuf,
    pub path: String,
}

pub fn parse_filter(input: &str) -> Filter {
    let mut filter = Filter::default();
    let mut text = Vec::new();
    for token in input.split_whitespace() {
        let parsed = match token.split_once(':') {
            Some(("ext", value)) => {
                let value = value.trim_start_matches('.').to_lowercase();
                (!value.is_empty()).then(|| filter.ext = Some(value))
            }
            Some(("git", value)) => GitFilter::parse(value).map(|git| filter.git = Some(git)),
            Some(("since", value)) => parse_duration(value).map(|since| filter.since = Some(since)),
            Some(("size", value)) => parse_size(value.strip_prefix('>').unwrap_or(value))
                .map(|size| filter.size_gt = Some(size)),
            _ => None,
        };
        if parsed.is_none() {
            text.push(token);
        }
    }
    filter.text = text.join(" ");
    filter
}

fn split_number_and_unit(text: &str) -> Option<(u64, &str)> {
    let digits_end = text
        .char_indices()
        .find(|(_, character)| !character.is_ascii_digit())
        .map_or(text.len(), |(index, _)| index);
    let number = text.get(..digits_end)?.parse::<u64>().ok()?;
    Some((number, text.get(digits_end..)?))
}

fn parse_duration(text: &str) -> Option<Duration> {
    let (number, unit) = split_number_and_unit(text)?;
    let seconds = match unit {
        "m" => number.checked_mul(60)?,
        "h" => number.checked_mul(3600)?,
        "d" => number.checked_mul(86400)?,
        _ => return None,
    };
    Some(Duration::from_secs(seconds))
}

fn parse_size(text: &str) -> Option<u64> {
    let (number, unit) = split_number_and_unit(text)?;
    match unit.to_lowercase().as_str() {
        "" => Some(number),
        "k" => number.checked_mul(1024),
        "m" => number.checked_mul(1024 * 1024),
        "g" => number.checked_mul(1024 * 1024 * 1024),
        _ => None,
    }
}

impl Filter {
    pub fn has_prefixes(&self) -> bool {
        self.ext.is_some() || self.git.is_some() || self.since.is_some() || self.size_gt.is_some()
    }

    /// Applies the prefix constraints only; fuzzy matching of `text` is the caller's job.
    pub fn matches(&self, facts: &EntryFacts, now: SystemTime) -> bool {
        if let Some(ext) = &self.ext {
            if facts.is_dir {
                return false;
            }
            let matches_ext = facts
                .path
                .rsplit_once('.')
                .is_some_and(|(_, entry_ext)| entry_ext.eq_ignore_ascii_case(ext));
            if !matches_ext {
                return false;
            }
        }
        if let Some(git) = self.git
            && !git.matches(&facts.git)
        {
            return false;
        }
        if let Some(since) = self.since {
            if facts.is_dir {
                return false;
            }
            let Some(mtime) = facts.mtime else {
                return false;
            };
            let age = now.duration_since(mtime).unwrap_or(Duration::ZERO);
            if age > since {
                return false;
            }
        }
        if let Some(size_gt) = self.size_gt
            && (facts.is_dir || facts.size <= size_gt)
        {
            return false;
        }
        true
    }
}

impl GitFilter {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "modified" => Some(Self::Modified),
            "added" => Some(Self::Added),
            "untracked" => Some(Self::Untracked),
            "conflict" => Some(Self::Conflict),
            _ => None,
        }
    }

    pub fn matches(self, summary: &GitSummary) -> bool {
        match self {
            Self::Modified => summary.worktree.modified > 0 || summary.index.modified > 0,
            Self::Added => summary.index.added > 0 || summary.worktree.added > 0,
            Self::Untracked => summary.untracked > 0,
            Self::Conflict => summary.conflict > 0,
        }
    }
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 3] = ["KB", "MB", "GB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = UNITS[0];
    for next_unit in &UNITS[1..] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next_unit;
    }
    format!("{value:.1} {unit}")
}

/// Files larger than this never get a line count in the hover popover.
pub const LINE_COUNT_MAX_BYTES: u64 = 1024 * 1024;

/// What the file-row hover popover shows for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub line_count: Option<u32>,
}

impl FileInfo {
    /// `size · modified age · N lines`, omitting the parts that are unknown.
    pub fn label(&self, now: SystemTime) -> String {
        let mut parts = vec![format_size(self.size)];
        if let Some(modified) = self.modified
            && let Ok(age) = now.duration_since(modified)
        {
            parts.push(format!("modified {}", format_age(age)));
        }
        if let Some(line_count) = self.line_count {
            let noun = if line_count == 1 { "line" } else { "lines" };
            parts.push(format!("{line_count} {noun}"));
        }
        parts.join(" · ")
    }
}

/// Coarse "3 minutes ago" style rendering of an age.
pub fn format_age(age: Duration) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;
    const MONTH: u64 = 30 * DAY;
    const YEAR: u64 = 365 * DAY;
    let seconds = age.as_secs();
    let (value, unit) = if seconds < MINUTE {
        return "just now".to_string();
    } else if seconds < HOUR {
        (seconds / MINUTE, "minute")
    } else if seconds < DAY {
        (seconds / HOUR, "hour")
    } else if seconds < WEEK {
        (seconds / DAY, "day")
    } else if seconds < MONTH {
        (seconds / WEEK, "week")
    } else if seconds < YEAR {
        (seconds / MONTH, "month")
    } else {
        (seconds / YEAR, "year")
    };
    let plural = if value == 1 { "" } else { "s" };
    format!("{value} {unit}{plural} ago")
}

/// Opens `path` without following a link and reads at most `limit + 1` bytes on the
/// calling thread, so run it on the background executor. Directories and special files
/// (a FIFO would otherwise block until something writes to it) are refused by the open.
pub async fn read_prefix_nofollow(fs: &dyn Fs, path: &Path, limit: u64) -> Result<Vec<u8>> {
    let reader = fs.open_sync_nofollow(path).await?;
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {path:?}"))?;
    Ok(bytes)
}

/// Number of lines in a text file, or `None` when the first 8 KiB contain a NUL byte
/// (the file is treated as binary).
pub fn count_lines(bytes: &[u8]) -> Option<u32> {
    const PROBE: usize = 8 * 1024;
    if bytes.iter().take(PROBE).any(|byte| *byte == 0) {
        return None;
    }
    if bytes.is_empty() {
        return Some(0);
    }
    let newlines = bytes.iter().filter(|byte| **byte == b'\n').count();
    let trailing = if bytes.ends_with(b"\n") { 0 } else { 1 };
    u32::try_from(newlines + trailing).ok()
}

impl FolderStats {
    pub fn label(&self) -> String {
        let noun = if self.files == 1 { "file" } else { "files" };
        format!("{} {noun} · {}", self.files, format_size(self.bytes))
    }
}

pub fn push_recent(list: &mut Vec<GrovePath>, item: GrovePath, cap: usize) {
    list.retain(|existing| *existing != item);
    list.insert(0, item);
    list.truncate(cap);
}

/// Returns whether `item` is in the list after the toggle.
pub fn toggle(list: &mut Vec<GrovePath>, item: GrovePath) -> bool {
    if let Some(index) = list.iter().position(|existing| *existing == item) {
        list.remove(index);
        false
    } else {
        list.push(item);
        true
    }
}

pub fn encode(list: &[GrovePath]) -> anyhow::Result<String> {
    Ok(serde_json::to_string(list)?)
}

pub fn decode(json: &str) -> Vec<GrovePath> {
    match serde_json::from_str(json) {
        Ok(list) => list,
        Err(error) => {
            log::warn!("ignoring malformed grove list: {error}");
            Vec::new()
        }
    }
}

pub fn kvp_key(kind: &str, workspace_id: i64) -> String {
    format!("grove:{kind}:{workspace_id}")
}

pub const KIT_KVP_KIND: &str = "kit";

/// How the Kit strip at the bottom of the panel was left: collapsed or not,
/// and its height in pixels when the user has dragged it (`None` keeps the
/// default third of the panel).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct KitLayout {
    pub collapsed: bool,
    pub height: Option<f32>,
}

pub fn encode_kit_layout(layout: &KitLayout) -> anyhow::Result<String> {
    Ok(serde_json::to_string(layout)?)
}

pub fn decode_kit_layout(json: &str) -> KitLayout {
    match serde_json::from_str(json) {
        Ok(layout) => layout,
        Err(error) => {
            log::warn!("ignoring malformed kit layout: {error}");
            KitLayout::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::status::TrackedSummary;

    fn grove_path(path: &str) -> GrovePath {
        GrovePath {
            worktree: PathBuf::from("/root"),
            path: path.to_string(),
        }
    }

    fn facts<'a>(path: &'a str, size: u64, age: Duration, now: SystemTime) -> EntryFacts<'a> {
        EntryFacts {
            path,
            is_dir: false,
            mtime: Some(now - age),
            size,
            git: GitSummary::UNCHANGED,
        }
    }

    #[test]
    fn plain_text_has_no_prefixes() {
        let filter = parse_filter("main rs");
        assert_eq!(filter.text, "main rs");
        assert_eq!(filter.ext, None);
        assert_eq!(filter.git, None);
        assert_eq!(filter.since, None);
        assert_eq!(filter.size_gt, None);
        assert!(!filter.has_prefixes());
    }

    #[test]
    fn prefixes_compose_and_leave_free_text() {
        let filter = parse_filter("ext:ts git:modified since:2h size:>1m panel");
        assert_eq!(filter.text, "panel");
        assert_eq!(filter.ext.as_deref(), Some("ts"));
        assert_eq!(filter.git, Some(GitFilter::Modified));
        assert_eq!(filter.since, Some(Duration::from_secs(2 * 3600)));
        assert_eq!(filter.size_gt, Some(1024 * 1024));
        assert!(filter.has_prefixes());
    }

    #[test]
    fn ext_prefix_strips_leading_dot_and_lowercases() {
        assert_eq!(parse_filter("ext:.TS").ext.as_deref(), Some("ts"));
        assert_eq!(parse_filter("ext:").ext, None);
    }

    #[test]
    fn duration_units() {
        assert_eq!(
            parse_filter("since:15m").since,
            Some(Duration::from_secs(900))
        );
        assert_eq!(
            parse_filter("since:3d").since,
            Some(Duration::from_secs(3 * 86400))
        );
        assert_eq!(parse_filter("since:3w").since, None);
        assert_eq!(parse_filter("since:").since, None);
    }

    #[test]
    fn size_units_and_optional_gt() {
        assert_eq!(parse_filter("size:>2k").size_gt, Some(2048));
        assert_eq!(parse_filter("size:1g").size_gt, Some(1024 * 1024 * 1024));
        assert_eq!(parse_filter("size:512").size_gt, Some(512));
        assert_eq!(parse_filter("size:>big").size_gt, None);
    }

    #[test]
    fn git_values_parse_and_unknown_is_kept_as_text() {
        assert_eq!(parse_filter("git:added").git, Some(GitFilter::Added));
        assert_eq!(
            parse_filter("git:untracked").git,
            Some(GitFilter::Untracked)
        );
        assert_eq!(parse_filter("git:conflict").git, Some(GitFilter::Conflict));
        let filter = parse_filter("git:weird");
        assert_eq!(filter.git, None);
        assert_eq!(filter.text, "git:weird");
    }

    #[test]
    fn ext_match_is_case_insensitive_and_excludes_directories() {
        let now = SystemTime::now();
        let filter = parse_filter("ext:rs");
        assert!(filter.matches(&facts("src/main.RS", 1, Duration::ZERO, now), now));
        assert!(!filter.matches(&facts("src/main.ts", 1, Duration::ZERO, now), now));
        let mut directory = facts("src.rs", 0, Duration::ZERO, now);
        directory.is_dir = true;
        assert!(!filter.matches(&directory, now));
        assert!(parse_filter("x").matches(&directory, now));
    }

    #[test]
    fn since_rejects_old_entries_and_entries_without_mtime() {
        let now = SystemTime::now();
        let filter = parse_filter("since:1h");
        assert!(filter.matches(&facts("a", 1, Duration::from_secs(600), now), now));
        assert!(!filter.matches(&facts("a", 1, Duration::from_secs(7200), now), now));
        let mut no_mtime = facts("a", 1, Duration::ZERO, now);
        no_mtime.mtime = None;
        assert!(!filter.matches(&no_mtime, now));
    }

    #[test]
    fn size_gt_is_strict() {
        let now = SystemTime::now();
        let filter = parse_filter("size:>1k");
        assert!(filter.matches(&facts("a", 1025, Duration::ZERO, now), now));
        assert!(!filter.matches(&facts("a", 1024, Duration::ZERO, now), now));
    }

    #[test]
    fn git_filters_match_summaries() {
        let modified_in_worktree = GitSummary {
            worktree: TrackedSummary {
                modified: 1,
                ..TrackedSummary::UNCHANGED
            },
            count: 1,
            ..GitSummary::UNCHANGED
        };
        let modified_in_index = GitSummary {
            index: TrackedSummary {
                modified: 1,
                ..TrackedSummary::UNCHANGED
            },
            count: 1,
            ..GitSummary::UNCHANGED
        };
        let added = GitSummary {
            index: TrackedSummary {
                added: 1,
                ..TrackedSummary::UNCHANGED
            },
            count: 1,
            ..GitSummary::UNCHANGED
        };
        assert!(GitFilter::Modified.matches(&modified_in_worktree));
        assert!(GitFilter::Modified.matches(&modified_in_index));
        assert!(!GitFilter::Modified.matches(&added));
        assert!(GitFilter::Added.matches(&added));
        assert!(GitFilter::Untracked.matches(&GitSummary::UNTRACKED));
        assert!(GitFilter::Conflict.matches(&GitSummary::CONFLICT));
        assert!(!GitFilter::Conflict.matches(&GitSummary::UNCHANGED));

        let now = SystemTime::now();
        let mut facts = facts("a", 1, Duration::ZERO, now);
        facts.git = added;
        assert!(parse_filter("git:added").matches(&facts, now));
        assert!(!parse_filter("git:untracked").matches(&facts, now));
    }

    #[test]
    fn format_size_boundaries() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(3 * 1024 * 1024 + 200 * 1024), "3.2 MB");
        assert_eq!(
            format_size(1024 * 1024 * 1024 + 100 * 1024 * 1024),
            "1.1 GB"
        );
    }

    #[test]
    fn format_age_units_and_plurals() {
        assert_eq!(format_age(Duration::from_secs(5)), "just now");
        assert_eq!(format_age(Duration::from_secs(60)), "1 minute ago");
        assert_eq!(format_age(Duration::from_secs(3 * 60)), "3 minutes ago");
        assert_eq!(format_age(Duration::from_secs(2 * 3600)), "2 hours ago");
        assert_eq!(format_age(Duration::from_secs(86_400)), "1 day ago");
        assert_eq!(format_age(Duration::from_secs(8 * 86_400)), "1 week ago");
        assert_eq!(format_age(Duration::from_secs(45 * 86_400)), "1 month ago");
        assert_eq!(format_age(Duration::from_secs(800 * 86_400)), "2 years ago");
    }

    #[test]
    fn count_lines_handles_trailing_newline_and_binary() {
        assert_eq!(count_lines(b""), Some(0));
        assert_eq!(count_lines(b"one"), Some(1));
        assert_eq!(count_lines(b"one\n"), Some(1));
        assert_eq!(count_lines(b"one\ntwo"), Some(2));
        assert_eq!(count_lines(b"one\ntwo\n"), Some(2));
        assert_eq!(count_lines(b"bin\0ary"), None);
    }

    #[test]
    fn file_info_label_omits_unknown_parts() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        let modified = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(10_000 - 120));
        let full = FileInfo {
            size: 2048,
            modified,
            line_count: Some(1),
        };
        assert_eq!(full.label(now), "2.0 KB · modified 2 minutes ago · 1 line");
        let bare = FileInfo {
            size: 10,
            modified: None,
            line_count: None,
        };
        assert_eq!(bare.label(now), "10 B");
    }

    #[test]
    fn folder_stats_label() {
        assert_eq!(FolderStats { files: 1, bytes: 6 }.label(), "1 file · 6 B");
        assert_eq!(
            FolderStats {
                files: 12,
                bytes: 3 * 1024 * 1024 + 200 * 1024
            }
            .label(),
            "12 files · 3.2 MB"
        );
    }

    #[test]
    fn push_recent_moves_to_front_dedupes_and_caps() {
        let mut list = Vec::new();
        for index in 0..12 {
            push_recent(&mut list, grove_path(&format!("file{index}")), RECENT_CAP);
        }
        assert_eq!(list.len(), RECENT_CAP);
        assert_eq!(list[0].path, "file11");
        assert_eq!(list[9].path, "file2");

        push_recent(&mut list, grove_path("file5"), RECENT_CAP);
        assert_eq!(list.len(), RECENT_CAP);
        assert_eq!(list[0].path, "file5");
        assert_eq!(list.iter().filter(|item| item.path == "file5").count(), 1);
    }

    #[test]
    fn toggle_adds_then_removes() {
        let mut list = Vec::new();
        assert!(toggle(&mut list, grove_path("a")));
        assert!(toggle(&mut list, grove_path("b")));
        assert_eq!(list.len(), 2);
        assert!(!toggle(&mut list, grove_path("a")));
        assert_eq!(list, vec![grove_path("b")]);
    }

    #[test]
    fn encode_decode_round_trip_and_invalid_json_is_empty() {
        let list = vec![grove_path("a"), grove_path("dir/b.rs")];
        let json = encode(&list).expect("encode");
        assert_eq!(decode(&json), list);
        assert!(decode("not json").is_empty());
    }

    #[test]
    fn kit_layout_round_trips_and_malformed_json_is_default() {
        let layout = KitLayout {
            collapsed: true,
            height: Some(180.5),
        };
        let json = encode_kit_layout(&layout).expect("encode");
        assert_eq!(decode_kit_layout(&json), layout);
        assert_eq!(decode_kit_layout("{}"), KitLayout::default());
        assert_eq!(decode_kit_layout("nope"), KitLayout::default());
        assert_eq!(kvp_key(KIT_KVP_KIND, 3), "grove:kit:3");
    }

    #[test]
    fn kvp_keys_are_scoped_per_workspace() {
        assert_eq!(kvp_key("pinned", 7), "grove:pinned:7");
        assert_ne!(kvp_key("pinned", 7), kvp_key("recent", 7));
        assert_ne!(kvp_key("pinned", 7), kvp_key("pinned", 8));
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn read_prefix_nofollow_refuses_links_and_never_waits_on_a_fifo(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::os::unix::ffi::OsStrExt as _;
        use std::sync::Arc;

        let directory = tempfile::tempdir().expect("tempdir");
        let real = directory.path().join("real.txt");
        std::fs::write(&real, "a\nb\n").expect("write");
        let link = directory.path().join("link.txt");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let fifo = directory.path().join("pipe");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).expect("cstring");
        // SAFETY: a valid NUL-terminated path; mkfifo has no other preconditions.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

        let real_fs: Arc<dyn Fs> = fs::RealFs::new(None, cx.executor());
        let (sender, receiver) = std::sync::mpsc::channel();
        // Reads run on their own thread so a regression that blocks on the FIFO shows up
        // as a timeout instead of hanging the whole test binary.
        std::thread::spawn(move || {
            let read = |path: &Path| {
                futures::executor::block_on(read_prefix_nofollow(real_fs.as_ref(), path, 1024))
                    .map_err(|error| error.to_string())
            };
            sender
                .send((read(&real), read(&link), read(&fifo)))
                .expect("send");
        });
        let (real, link, fifo) = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("reading a FIFO must return without a writer");
        assert_eq!(real.expect("a regular file reads"), b"a\nb\n");
        assert!(link.is_err(), "a symlink is never followed");
        let error = fifo.expect_err("a FIFO is refused");
        assert!(error.contains("not a regular file"), "{error}");
    }
}
