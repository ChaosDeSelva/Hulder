//! Pure helpers for the PDF tab: the text-to-Markdown formatter and the
//! zoom, fit, and page-layout arithmetic.

use std::ops::Range;

pub const MIN_ZOOM: f32 = 0.25;
pub const MAX_ZOOM: f32 = 8.0;
pub const ZOOM_STEP: f32 = 1.25;
/// Vertical space between pages and around the page column, in logical pixels.
pub const PAGE_GAP: f32 = 16.0;
/// Horizontal margin kept on each side of a fit-width page.
pub const PAGE_MARGIN: f32 = 24.0;

/// One `## Page N` section per page, in order, even when a page has no text
/// (scanned pages) so numbering never drifts.
pub fn pages_to_markdown(pages: &[String]) -> String {
    let mut markdown = String::new();
    for (index, text) in pages.iter().enumerate() {
        if index > 0 {
            markdown.push('\n');
        }
        markdown.push_str(&format!("## Page {}\n\n", index + 1));
        let text = text.trim();
        if !text.is_empty() {
            markdown.push_str(text);
            markdown.push('\n');
        }
    }
    markdown
}

/// Largest file the viewer loads; bigger ones are refused before parsing.
pub const MAX_PDF_BYTES: usize = 200 * 1024 * 1024;
/// Largest bitmap drawn for one page, in pixels (32 megapixels).
pub const MAX_PAGE_PIXELS: f32 = 32_000_000.0;
/// Most extracted text kept across all pages.
pub const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;
/// Most text kept from one page, so a single huge page cannot hold the
/// whole budget in one allocation.
pub const MAX_PAGE_TEXT_BYTES: usize = 512 * 1024;
/// Most Markdown a clipping may carry: the store's cap minus headroom for
/// the front matter and the notice.
pub const CLIPPING_TEXT_BYTES: usize = clippings::MAX_MARKDOWN_BYTES as usize - 8 * 1024;
pub const TRUNCATION_NOTICE: &str =
    "\n\n_The document's text was cut here: it is longer than the 1 MB clipping limit._\n";
/// Most rasterized pages kept in memory per tab.
pub const MAX_CACHED_PAGES: usize = 12;

pub fn reject_oversized(byte_length: usize, name: &str) -> anyhow::Result<()> {
    if byte_length > MAX_PDF_BYTES {
        anyhow::bail!(
            "{name} is {} MB; Hulder opens PDFs up to {} MB",
            byte_length / (1024 * 1024),
            MAX_PDF_BYTES / (1024 * 1024)
        );
    }
    Ok(())
}

/// Highest zoom at which every page's bitmap stays under `MAX_PAGE_PIXELS`
/// at `scale_factor` device pixels per logical pixel.
pub fn max_zoom_for_pages(page_sizes: &[(f32, f32)], scale_factor: f32) -> f32 {
    let largest_area = page_sizes
        .iter()
        .map(|(width, height)| width * height)
        .fold(0.0_f32, f32::max);
    if largest_area <= 0.0 || scale_factor <= 0.0 {
        return MAX_ZOOM;
    }
    let max_scale = (MAX_PAGE_PIXELS / largest_area).sqrt();
    (max_scale / scale_factor).clamp(MIN_ZOOM, MAX_ZOOM)
}

fn truncate_at_char_boundary(text: &mut String, max_bytes: usize) {
    let mut cut = max_bytes.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
}

pub fn cap_page_text(mut text: String, max_bytes: usize) -> String {
    if text.len() > max_bytes {
        truncate_at_char_boundary(&mut text, max_bytes);
    }
    text
}

/// Cuts `markdown` to `max_bytes` and appends `TRUNCATION_NOTICE`; the flag
/// says whether anything was cut.
pub fn truncate_for_clipping(mut markdown: String, max_bytes: usize) -> (String, bool) {
    if markdown.len() <= max_bytes {
        return (markdown, false);
    }
    truncate_at_char_boundary(&mut markdown, max_bytes);
    markdown.push_str(TRUNCATION_NOTICE);
    (markdown, true)
}

/// Keeps page texts until `max_bytes` have been collected; later pages are
/// blanked rather than dropped so page numbering survives.
pub fn cap_text_total(mut texts: Vec<String>, max_bytes: usize) -> Vec<String> {
    let mut remaining = max_bytes;
    for text in &mut texts {
        if text.len() <= remaining {
            remaining -= text.len();
        } else {
            let mut cut = remaining;
            while cut > 0 && !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            remaining = 0;
        }
    }
    texts
}

/// Keys to evict from a cache that must hold at most `capacity` entries,
/// least recently used first, never one that `keep` claims. `recency` lists
/// keys oldest to newest.
pub fn lru_evictions<K: Copy + Eq>(
    recency: &[K],
    capacity: usize,
    keep: impl Fn(&K) -> bool,
) -> Vec<K> {
    let mut excess = recency.len().saturating_sub(capacity);
    let mut evictions = Vec::new();
    for key in recency {
        if excess == 0 {
            break;
        }
        if keep(key) {
            continue;
        }
        evictions.push(*key);
        excess -= 1;
    }
    evictions
}

/// Heights of the blank blocks that stand in for the pages before and after
/// `laid_out` in the page column, so only those pages become elements. Each
/// block is one flex child, so the column gap on its far side is folded in.
pub fn spacer_heights(layout: &[Range<f32>], laid_out: Range<usize>) -> (f32, f32) {
    let (Some(first), Some(last)) = (layout.get(laid_out.start), layout.last()) else {
        return (0.0, 0.0);
    };
    let before = if laid_out.start == 0 {
        0.0
    } else {
        first.start - 2.0 * PAGE_GAP
    };
    let after = match layout.get(laid_out.end.saturating_sub(1)) {
        Some(last_laid_out) if laid_out.end < layout.len() => {
            last.end - last_laid_out.end - PAGE_GAP
        }
        _ => 0.0,
    };
    (before.max(0.0), after.max(0.0))
}

pub fn clamp_zoom(zoom: f32) -> f32 {
    if zoom.is_finite() {
        zoom.clamp(MIN_ZOOM, MAX_ZOOM)
    } else {
        1.0
    }
}

/// Zoom that makes a page of `page_width_points` fill `container_width`
/// minus the margins. One PDF point maps to one logical pixel at zoom 1.
pub fn fit_width_zoom(container_width: f32, page_width_points: f32) -> f32 {
    if page_width_points <= 0.0 || container_width <= 0.0 {
        return 1.0;
    }
    let available = (container_width - 2.0 * PAGE_MARGIN).max(1.0);
    clamp_zoom(available / page_width_points)
}

/// Bitmap size for a page at `scale` device pixels per point; never zero in
/// either dimension.
pub fn page_pixel_size(page_points: (f32, f32), scale: f32) -> (u32, u32) {
    let width = (page_points.0 * scale).ceil().max(1.0) as u32;
    let height = (page_points.1 * scale).ceil().max(1.0) as u32;
    (width, height)
}

/// Cache key for a rasterization scale: thousandths of a device pixel per point.
pub fn scale_key(scale: f32) -> u32 {
    (scale * 1000.0).round().max(1.0) as u32
}

/// Top offset and height of every page in logical pixels, stacked vertically
/// with `PAGE_GAP` between them and above the first page.
pub fn page_layout(page_sizes: &[(f32, f32)], zoom: f32) -> Vec<Range<f32>> {
    let mut top = PAGE_GAP;
    page_sizes
        .iter()
        .map(|(_, height_points)| {
            let height = height_points * zoom;
            let range = top..top + height;
            top += height + PAGE_GAP;
            range
        })
        .collect()
}

/// Pages intersecting the viewport, widened by `lookahead` pages on each side
/// so a scroll never shows a page before its bitmap exists.
pub fn visible_pages(
    layout: &[Range<f32>],
    scroll_top: f32,
    viewport_height: f32,
    lookahead: usize,
) -> Range<usize> {
    if layout.is_empty() {
        return 0..0;
    }
    let viewport_bottom = scroll_top + viewport_height.max(1.0);
    let first = layout
        .iter()
        .position(|page| page.end > scroll_top)
        .unwrap_or(layout.len() - 1);
    let last = layout
        .iter()
        .rposition(|page| page.start < viewport_bottom)
        .unwrap_or(first)
        .max(first);
    first.saturating_sub(lookahead)..(last + 1 + lookahead).min(layout.len())
}

/// The page the reader is "on": the first page whose bottom edge is below the
/// top third of the viewport.
pub fn current_page(layout: &[Range<f32>], scroll_top: f32, viewport_height: f32) -> usize {
    let anchor = scroll_top + viewport_height.max(0.0) / 3.0;
    layout
        .iter()
        .position(|page| page.end > anchor)
        .unwrap_or(layout.len().saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_has_one_heading_per_page_including_empty_pages() {
        let pages = vec![" first page ".to_string(), String::new(), "third".to_string()];
        assert_eq!(
            pages_to_markdown(&pages),
            "## Page 1\n\nfirst page\n\n## Page 2\n\n\n## Page 3\n\nthird\n"
        );
        assert_eq!(pages_to_markdown(&[]), "");
    }

    #[test]
    fn fit_width_uses_the_container_minus_margins() {
        let zoom = fit_width_zoom(612.0 + 2.0 * PAGE_MARGIN, 612.0);
        assert!((zoom - 1.0).abs() < 1e-6, "{zoom}");
        assert_eq!(fit_width_zoom(0.0, 612.0), 1.0);
        assert_eq!(fit_width_zoom(800.0, 0.0), 1.0);
        assert_eq!(fit_width_zoom(100_000.0, 10.0), MAX_ZOOM);
        assert_eq!(fit_width_zoom(50.0, 10_000.0), MIN_ZOOM);
    }

    #[test]
    fn pixel_size_rounds_up_and_never_hits_zero() {
        assert_eq!(page_pixel_size((612.0, 792.0), 2.0), (1224, 1584));
        assert_eq!(page_pixel_size((10.5, 10.5), 1.0), (11, 11));
        assert_eq!(page_pixel_size((0.0, 0.0), 1.0), (1, 1));
        assert_eq!(scale_key(1.0), 1000);
        assert_eq!(scale_key(2.5), 2500);
        assert_eq!(scale_key(0.0), 1);
        assert_eq!(clamp_zoom(f32::NAN), 1.0);
        assert_eq!(clamp_zoom(100.0), MAX_ZOOM);
    }

    #[test]
    fn oversized_files_are_refused_with_the_limit_in_the_message() {
        assert!(reject_oversized(MAX_PDF_BYTES, "ok.pdf").is_ok());
        let error = reject_oversized(MAX_PDF_BYTES + 1, "huge.pdf").expect_err("over the cap");
        assert!(error.to_string().contains("huge.pdf is 200 MB"), "{error}");
        assert!(error.to_string().contains("up to 200 MB"), "{error}");
    }

    #[test]
    fn bitmap_cap_lowers_the_zoom_for_large_pages() {
        // A letter page at 2x needs 4x the pixels of a 1x render.
        let letter = [(612.0, 792.0)];
        let zoom = max_zoom_for_pages(&letter, 2.0);
        let pixels = 612.0 * 792.0 * (zoom * 2.0).powi(2);
        assert!(pixels <= MAX_PAGE_PIXELS + 1.0, "{pixels}");
        assert!(zoom > 1.0 && zoom < MAX_ZOOM, "{zoom}");
        // A billboard-sized page cannot even be shown at 1:1, but the zoom
        // never drops under the minimum: below it the fit-width zoom and the
        // cap disagree every frame and the viewer re-renders forever.
        let billboard = max_zoom_for_pages(&[(20_000.0, 20_000.0)], 1.0);
        assert!(billboard < 1.0 && billboard >= MIN_ZOOM, "{billboard}");
        assert_eq!(max_zoom_for_pages(&[(200_000.0, 200_000.0)], 1.0), MIN_ZOOM);
        assert_eq!(max_zoom_for_pages(&[], 2.0), MAX_ZOOM);
        assert_eq!(max_zoom_for_pages(&[(1.0, 1.0)], 1.0), MAX_ZOOM);
    }

    #[test]
    fn text_cap_blanks_later_pages_and_respects_char_boundaries() {
        let texts = vec!["abcd".to_string(), "héllo".to_string(), "more".to_string()];
        assert_eq!(
            cap_text_total(texts, 6),
            vec!["abcd".to_string(), "h".to_string(), String::new()]
        );
        assert_eq!(cap_text_total(vec!["abc".to_string()], 10), vec!["abc".to_string()]);
    }

    #[test]
    fn lru_evicts_oldest_beyond_capacity() {
        assert_eq!(lru_evictions(&[1, 2, 3, 4], 2, |_| false), vec![1, 2]);
        assert!(lru_evictions(&[1, 2], 2, |_| false).is_empty());
        assert!(lru_evictions::<u32>(&[], 2, |_| false).is_empty());
    }

    #[test]
    fn lru_never_evicts_a_kept_key_even_over_capacity() {
        // Pages on screen must stay cached, else they are evicted and
        // re-requested every frame.
        assert_eq!(lru_evictions(&[1, 2, 3, 4], 2, |key| *key == 1), vec![2, 3]);
        assert!(lru_evictions(&[1, 2, 3, 4], 2, |_| true).is_empty());
        assert_eq!(lru_evictions(&[1, 2, 3, 4, 5], 1, |key| *key % 2 == 0), vec![1, 3, 5]);
        assert_eq!(lru_evictions(&[1, 2, 3], 2, |key| *key == 1), vec![2]);
    }

    #[test]
    fn spacers_stand_in_for_pages_outside_the_laid_out_range() {
        let sizes: Vec<(f32, f32)> = (0..5).map(|_| (100.0, 100.0)).collect();
        let layout = page_layout(&sizes, 1.0);
        let page_pitch = 100.0 + PAGE_GAP;
        assert_eq!(spacer_heights(&layout, 0..5), (0.0, 0.0));
        assert_eq!(spacer_heights(&layout, 0..2), (0.0, 3.0 * page_pitch - PAGE_GAP));
        assert_eq!(spacer_heights(&layout, 2..5), (2.0 * page_pitch - PAGE_GAP, 0.0));
        assert_eq!(
            spacer_heights(&layout, 1..4),
            (page_pitch - PAGE_GAP, page_pitch - PAGE_GAP)
        );
        assert_eq!(spacer_heights(&[], 0..0), (0.0, 0.0));
    }

    #[test]
    fn page_text_is_capped_per_page_too() {
        assert_eq!(cap_page_text("héllo".to_string(), 2), "h");
        assert_eq!(cap_page_text("abc".to_string(), 3), "abc");
        assert_eq!(cap_page_text("abc".to_string(), 0), "");
        assert!(MAX_PAGE_TEXT_BYTES < MAX_TEXT_BYTES);
    }

    #[test]
    fn clipping_text_is_cut_to_the_clipping_cap_with_a_notice() {
        let (text, truncated) = truncate_for_clipping("short".to_string(), 100);
        assert_eq!(text, "short");
        assert!(!truncated);
        let long = "é".repeat(100);
        let (text, truncated) = truncate_for_clipping(long, 51);
        assert!(truncated);
        assert!(text.len() <= 51 + TRUNCATION_NOTICE.len(), "{}", text.len());
        assert!(text.starts_with(&"é".repeat(25)));
        assert!(text.ends_with(TRUNCATION_NOTICE));
        assert!(MAX_TEXT_BYTES > CLIPPING_TEXT_BYTES);
        assert!(CLIPPING_TEXT_BYTES < clippings::MAX_MARKDOWN_BYTES as usize);
    }

    #[test]
    fn layout_stacks_pages_with_gaps() {
        let layout = page_layout(&[(100.0, 200.0), (100.0, 100.0)], 2.0);
        assert_eq!(layout, vec![PAGE_GAP..PAGE_GAP + 400.0, 2.0 * PAGE_GAP + 400.0..2.0 * PAGE_GAP + 600.0]);
    }

    #[test]
    fn visible_and_current_pages_follow_the_scroll_position() {
        let sizes: Vec<(f32, f32)> = (0..10).map(|_| (100.0, 100.0)).collect();
        let layout = page_layout(&sizes, 1.0);
        let page_pitch = 100.0 + PAGE_GAP;

        assert_eq!(visible_pages(&layout, 0.0, 150.0, 0), 0..2);
        assert_eq!(visible_pages(&layout, 0.0, 150.0, 1), 0..3);
        assert_eq!(visible_pages(&layout, 5.0 * page_pitch, 150.0, 1), 4..8);
        assert_eq!(visible_pages(&layout, 100.0 * page_pitch, 150.0, 1), 8..10);
        assert_eq!(visible_pages(&[], 0.0, 150.0, 1), 0..0);

        assert_eq!(current_page(&layout, 0.0, 300.0), 0);
        assert_eq!(current_page(&layout, 3.0 * page_pitch + 50.0, 300.0), 4);
        assert_eq!(current_page(&layout, 100.0 * page_pitch, 300.0), 9);
    }
}
