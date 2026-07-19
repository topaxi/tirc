//! OSC 8 terminal hyperlinks for URLs in rendered messages.
//!
//! The wrapper (`super::wrap`) splits long URLs across visual lines, which
//! breaks terminals' implicit URL detection (it regexes visible rows, and a
//! wrapped URL is never a match). Explicit OSC 8 hyperlinks fix this: the link
//! target travels out of band, so every fragment of a wrapped URL stays
//! clickable, and a shared `id=` parameter lets the terminal treat the
//! fragments as one link on hover.
//!
//! The pipeline has three stages:
//! 1. [`tag_link_spans`] splits formatted spans so each URL is its own span,
//!    marked by an index stored in the style's otherwise-unused
//!    `underline_color` channel (`Color::Indexed`). Only fg/bg are settable
//!    from Lua themes, so the channel cannot collide, and `wrap` preserves the
//!    full `Style` across wrap points, so the marker survives wrapping for
//!    free.
//! 2. Rendering proceeds as usual; the marker lands in the frame buffer's
//!    cell styles.
//! 3. [`apply_hyperlinks`] runs as a post-pass over the finished buffer (still
//!    before flush), wraps each horizontal run of marked cells in OSC 8
//!    open/close escapes, and resets the marker so SGR 58 is never emitted.

use std::borrow::Cow;
use std::num::NonZeroU16;

use linkify::{LinkFinder, LinkKind};
use ratatui::buffer::{Buffer, Cell, CellDiffOption, CellWidth};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

/// `Color::Indexed` is a `u8`, so at most 256 distinct URLs can be marked per
/// frame. URLs beyond the cap render as plain text.
pub(crate) const MAX_FRAME_LINKS: usize = 256;

/// URLs longer than this are not hyperlinked; terminals commonly truncate or
/// reject oversized OSC payloads.
const MAX_URL_LEN: usize = 2048;

/// Splits `spans` so each http(s) URL becomes its own span carrying the link
/// marker (plus `UNDERLINED` as a visual affordance); all other text keeps its
/// original span and style. `urls` is the frame-local intern table: the index
/// of a URL in it is the `Color::Indexed` value stored in the marker, which
/// [`apply_hyperlinks`] resolves back after rendering.
pub(crate) fn tag_link_spans<'a>(spans: Vec<Span<'a>>, urls: &mut Vec<String>) -> Vec<Span<'a>> {
    let mut finder = LinkFinder::new();
    finder.kinds(&[LinkKind::Url]);

    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        // URL detection mirrors `preview::extract_urls` (same finder config and
        // scheme filter) so link boundaries, including linkify's trailing
        // punctuation handling, match the preview pipeline exactly.
        let links: Vec<(usize, usize)> = finder
            .links(&span.content)
            .filter(|link| is_linkable(link.as_str()))
            .map(|link| (link.start(), link.end()))
            .collect();

        if links.is_empty() {
            out.push(span);
            continue;
        }

        let mut cursor = 0;
        for (start, end) in links {
            if start > cursor {
                out.push(Span::styled(
                    slice_cow(&span.content, cursor, start),
                    span.style,
                ));
            }
            let style = match intern_url(urls, &span.content[start..end]) {
                Some(index) => marker_style(span.style, index),
                // Intern table full: render the URL as plain text.
                None => span.style,
            };
            out.push(Span::styled(slice_cow(&span.content, start, end), style));
            cursor = end;
        }
        if cursor < span.content.len() {
            out.push(Span::styled(
                slice_cow(&span.content, cursor, span.content.len()),
                span.style,
            ));
        }
    }
    out
}

/// Whether a detected link should be hyperlinked: http(s) only (mirroring
/// `preview::extract_urls`), bounded length, and no control characters that
/// could terminate or corrupt the OSC sequence.
fn is_linkable(url: &str) -> bool {
    (url.starts_with("http://") || url.starts_with("https://"))
        && url.len() <= MAX_URL_LEN
        && !url.chars().any(|ch| ch.is_ascii_control())
}

/// Returns the intern-table index for `url`, reusing an existing entry or
/// appending. `None` when the table is full ([`MAX_FRAME_LINKS`]).
fn intern_url(urls: &mut Vec<String>, url: &str) -> Option<u8> {
    if let Some(index) = urls.iter().position(|u| u == url) {
        return Some(index as u8);
    }
    if urls.len() >= MAX_FRAME_LINKS {
        return None;
    }
    urls.push(url.to_string());
    Some((urls.len() - 1) as u8)
}

/// The base style plus the link marker (`underline_color = Indexed(index)`)
/// and an underline for visual affordance.
fn marker_style(base: Style, index: u8) -> Style {
    base.add_modifier(Modifier::UNDERLINED)
        .underline_color(Color::Indexed(index))
}

/// `Some(index)` iff the cell carries the link marker and the index resolves
/// into the intern table.
fn marked_index(cell: &Cell, url_count: usize) -> Option<u8> {
    match cell.underline_color {
        Color::Indexed(index) if (index as usize) < url_count => Some(index),
        _ => None,
    }
}

/// Slices span content, keeping borrowed content borrowed.
fn slice_cow<'a>(content: &Cow<'a, str>, start: usize, end: usize) -> Cow<'a, str> {
    match content {
        Cow::Borrowed(s) => Cow::Borrowed(&s[start..end]),
        Cow::Owned(s) => Cow::Owned(s[start..end].to_string()),
    }
}

/// Scans `area` for horizontal runs of cells marked by [`tag_link_spans`] and
/// wraps each run in OSC 8 open/close escapes: the run's first cell symbol is
/// prefixed with `ESC ]8;id=l<i>;<url> ST` and its last cell symbol is
/// suffixed with `ESC ]8;; ST` (a single-cell run gets both). The `id`
/// parameter is stable per URL within the frame, so a link wrapped across
/// rows is one link to the terminal.
///
/// Rewritten cells get `CellDiffOption::ForcedWidth` with the symbol's real
/// display width - the diff would otherwise compute the width from the escape
/// bytes and skip the rest of the row (same technique as `draw_raw_image` in
/// the renderer). Every marked cell has its `underline_color` reset so the
/// marker never reaches the terminal as SGR 58.
///
/// Returns one hit box per rewritten run - `(Rect, url)` covering the run's
/// cells on its row - so the input handler can resolve a click position back to
/// a URL. A link wrapped across rows yields one entry per row; because this runs
/// after every overlay has `Clear`ed its cells, runs hidden under a popup are
/// not recorded.
pub(crate) fn apply_hyperlinks(
    buf: &mut Buffer,
    area: Rect,
    urls: &[String],
) -> Vec<(Rect, String)> {
    if urls.is_empty() {
        return Vec::new();
    }
    let area = area.intersection(buf.area);
    let mut hits = Vec::new();

    for y in area.top()..area.bottom() {
        let mut x = area.left();
        while x < area.right() {
            let Some(index) = buf.cell((x, y)).and_then(|c| marked_index(c, urls.len())) else {
                x += 1;
                continue;
            };

            let start = x;
            let mut end = x;
            while end + 1 < area.right()
                && buf
                    .cell((end + 1, y))
                    .is_some_and(|c| marked_index(c, urls.len()) == Some(index))
            {
                end += 1;
            }

            for cx in start..=end {
                if let Some(cell) = buf.cell_mut((cx, y)) {
                    cell.underline_color = Color::Reset;
                }
            }

            let url = &urls[index as usize];
            rewrite_symbol(buf, (start, y), |sym| {
                format!("\x1b]8;id=l{index};{url}\x1b\\{sym}")
            });
            rewrite_symbol(buf, (end, y), |sym| format!("{sym}\x1b]8;;\x1b\\"));

            hits.push((Rect::new(start, y, end - start + 1, 1), url.clone()));

            x = end + 1;
        }
    }

    hits
}

/// Replaces a cell's symbol via `f`, forcing the diff width to the symbol's
/// real display width (computed before the rewrite; an already-forced width is
/// kept, so the close-escape rewrite on a single-cell run stays correct).
fn rewrite_symbol(buf: &mut Buffer, pos: (u16, u16), f: impl FnOnce(&str) -> String) {
    if let Some(cell) = buf.cell_mut(pos) {
        let width = cell.cell_width().max(1);
        let symbol = f(cell.symbol());
        cell.set_symbol(&symbol)
            .set_diff_option(CellDiffOption::ForcedWidth(
                NonZeroU16::new(width).expect("width is clamped to at least 1"),
            ));
    }
}

/// Removes OSC 8 sequences (`ESC ]8; ... ST` or BEL-terminated) from `text`.
/// Used when assembling yank/selection text from the rendered frame, whose
/// cell symbols contain the escapes injected by [`apply_hyperlinks`].
pub(crate) fn strip_osc8(text: &str) -> Cow<'_, str> {
    const OSC8: &str = "\x1b]8;";

    if !text.contains(OSC8) {
        return Cow::Borrowed(text);
    }

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OSC8) {
        out.push_str(&rest[..start]);
        let after = &rest[start + OSC8.len()..];
        if let Some(end) = after.find("\x1b\\") {
            rest = &after[end + 2..];
        } else if let Some(end) = after.find('\x07') {
            rest = &after[end + 1..];
        } else {
            // Unterminated sequence: drop the remainder.
            rest = "";
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Line;

    fn tag<'a>(text: &'a str, urls: &mut Vec<String>) -> Vec<Span<'a>> {
        tag_link_spans(vec![Span::raw(text)], urls)
    }

    #[test]
    fn splits_url_into_marked_span() {
        let mut urls = Vec::new();
        let spans = tag("see https://example.com/a?x=1, ok", &mut urls);

        assert_eq!(urls, vec!["https://example.com/a?x=1".to_string()]);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "see ");
        assert_eq!(spans[1].content, "https://example.com/a?x=1");
        // linkify excludes the trailing comma, matching preview::extract_urls.
        assert_eq!(spans[2].content, ", ok");
        assert_eq!(
            super::super::preview::extract_urls("see https://example.com/a?x=1, ok"),
            urls
        );

        assert_eq!(spans[1].style.underline_color, Some(Color::Indexed(0)));
        assert!(spans[1].style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(spans[0].style, Style::default());
        assert_eq!(spans[2].style, Style::default());
    }

    #[test]
    fn keeps_base_style_on_url_span() {
        let mut urls = Vec::new();
        let spans = tag_link_spans(
            vec![Span::styled(
                "https://example.com",
                Style::default().fg(Color::Blue),
            )],
            &mut urls,
        );
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].style.fg, Some(Color::Blue));
        assert_eq!(spans[0].style.underline_color, Some(Color::Indexed(0)));
    }

    #[test]
    fn ignores_non_http_schemes_and_plain_text() {
        let mut urls = Vec::new();
        let spans = tag("ftp://example.com file://etc nothing", &mut urls);
        assert!(urls.is_empty());
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].style, Style::default());
    }

    #[test]
    fn dedupes_urls_across_spans() {
        let mut urls = Vec::new();
        let first = tag("https://example.com", &mut urls);
        let second = tag("again https://example.com", &mut urls);

        assert_eq!(urls.len(), 1);
        assert_eq!(
            first[0].style.underline_color,
            second[1].style.underline_color
        );
    }

    #[test]
    fn urls_beyond_the_cap_stay_untagged() {
        let mut urls = Vec::new();
        for i in 0..MAX_FRAME_LINKS {
            let text = format!("https://example.com/{i}");
            let spans = tag(&text, &mut urls);
            assert!(spans[0].style.underline_color.is_some());
        }
        let spans = tag("https://example.com/overflow", &mut urls);
        assert_eq!(urls.len(), MAX_FRAME_LINKS);
        assert_eq!(spans[0].style.underline_color, None);
    }

    #[test]
    fn marker_survives_wrapping() {
        use super::super::wrap::{wrap_line, Options};

        let mut urls = Vec::new();
        let spans = tag(
            "look at https://example.com/a/very/long/path/segment",
            &mut urls,
        );
        let line = Line::from(spans);
        let wrapped = wrap_line(
            &line,
            Options {
                width: 10,
                initial_indent: Box::new([]),
                subsequent_indent: Box::new([]),
                break_words: true,
            },
        );

        assert!(wrapped.lines.len() > 1, "the URL must actually wrap");
        let marked_fragments: Vec<&str> = wrapped
            .lines
            .iter()
            .flat_map(|l| &l.spans)
            .filter(|s| s.style.underline_color == Some(Color::Indexed(0)))
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            marked_fragments.len() > 1,
            "the URL is split into multiple fragments"
        );
        assert_eq!(
            marked_fragments.concat(),
            "https://example.com/a/very/long/path/segment",
            "every fragment of the URL keeps the marker"
        );
    }

    #[test]
    fn apply_hyperlinks_wraps_runs_in_osc8() {
        let urls = vec!["https://e.com/abcdefghij".to_string()];
        let area = Rect::new(0, 0, 10, 2);
        let mut buf = Buffer::empty(area);
        // The URL's last 10 chars wrapped onto row 1; both rows are marked.
        buf.set_line(
            0,
            0,
            &Line::styled("https://e.", marker_style(Style::default(), 0)),
            10,
        );
        buf.set_line(
            0,
            1,
            &Line::styled("com/abcdef", marker_style(Style::default(), 0)),
            10,
        );

        let hits = apply_hyperlinks(&mut buf, area, &urls);

        // One hit box per wrapped row, each spanning the full marked run and
        // carrying the whole URL.
        assert_eq!(
            hits,
            vec![
                (Rect::new(0, 0, 10, 1), urls[0].clone()),
                (Rect::new(0, 1, 10, 1), urls[0].clone()),
            ]
        );

        for y in 0..2 {
            let first = buf.cell((0, y)).unwrap();
            let last = buf.cell((9, y)).unwrap();
            assert!(
                first
                    .symbol()
                    .starts_with("\x1b]8;id=l0;https://e.com/abcdefghij\x1b\\"),
                "row {y} opens the hyperlink"
            );
            assert!(
                last.symbol().ends_with("\x1b]8;;\x1b\\"),
                "row {y} closes the hyperlink"
            );
            for x in 0..10 {
                let cell = buf.cell((x, y)).unwrap();
                assert_eq!(
                    cell.underline_color,
                    Color::Reset,
                    "marker cleared at ({x},{y})"
                );
                if x == 0 || x == 9 {
                    assert_eq!(
                        cell.diff_option,
                        CellDiffOption::ForcedWidth(NonZeroU16::new(1).unwrap()),
                        "rewritten cell keeps its real display width"
                    );
                } else {
                    assert!(
                        !cell.symbol().contains('\x1b'),
                        "interior cells are untouched"
                    );
                }
            }
        }
    }

    #[test]
    fn apply_hyperlinks_single_cell_run_gets_both_escapes() {
        let urls = vec!["https://e.com".to_string()];
        let area = Rect::new(0, 0, 3, 1);
        let mut buf = Buffer::empty(area);
        buf.set_line(
            0,
            0,
            &Line::styled("x", marker_style(Style::default(), 0)),
            3,
        );

        let hits = apply_hyperlinks(&mut buf, area, &urls);
        assert_eq!(hits, vec![(Rect::new(0, 0, 1, 1), urls[0].clone())]);

        let symbol = buf.cell((0, 0)).unwrap().symbol().to_string();
        assert_eq!(symbol, "\x1b]8;id=l0;https://e.com\x1b\\x\x1b]8;;\x1b\\");
    }

    #[test]
    fn apply_hyperlinks_ignores_unmarked_indexed_colors_out_of_range() {
        // A cell whose underline_color indexes past the intern table is not a
        // marker (e.g. stale or theme-produced); it must be left alone.
        let urls = vec!["https://e.com".to_string()];
        let area = Rect::new(0, 0, 2, 1);
        let mut buf = Buffer::empty(area);
        buf.set_line(
            0,
            0,
            &Line::styled("ab", Style::default().underline_color(Color::Indexed(5))),
            2,
        );

        let hits = apply_hyperlinks(&mut buf, area, &urls);
        assert!(hits.is_empty(), "an out-of-range marker is not a link hit");

        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "a");
        assert_eq!(buf.cell((0, 0)).unwrap().underline_color, Color::Indexed(5));
    }

    #[test]
    fn strip_osc8_removes_open_and_close() {
        let text = "\x1b]8;id=l0;https://e.com\x1b\\https://e.\x1b]8;;\x1b\\ tail";
        assert_eq!(strip_osc8(text), "https://e. tail");
        // Untouched text stays borrowed.
        assert!(matches!(strip_osc8("plain"), Cow::Borrowed("plain")));
    }
}
