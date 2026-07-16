use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use mlua::LuaSerdeExt;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect, Size},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListDirection, ListItem, ListState, Paragraph},
};
use ratatui_image::{protocol::Protocol, Image};
use tokio::sync::mpsc::UnboundedSender;
use tui_input::Input;

use tracing::Level;

use crate::backends::BackendInfo;
use crate::core::{AttachmentKind, BufferId, ChatEvent, EventId, TargetId};
use crate::logging::LogLine;
use crate::lua::date_time::date_time_to_table;
use crate::ui::{
    ChatBuffer, ConnectionStatus, LayoutMap, Member, Mode, ReactionHit, State, StoredMessage,
    ViewState,
};

use super::lua::{to_lua_event, to_lua_user, STYLE_MARKER};
use super::preview::{extract_urls, LinkPreview, PreviewRequest, PreviewResult};
use super::tmux::{wrap_passthrough, wrap_passthrough_positioned, PaneOrigin};
use super::wrap::wrap_line;

/// How the buffer bar scrolls to keep the focused tab visible.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BarScrollMode {
    /// Scroll the minimum amount to bring the focused tab into view (default).
    #[default]
    Follow,
    /// Always center the focused tab in the bar.
    Center,
}

/// Computes the horizontal scroll offset for the buffer bar so the focused tab
/// is visible (or centered, depending on `mode`). Returns 0 when all tabs fit
/// without scrolling. `prev_scroll` is the offset from the previous frame and
/// is only used by `Follow` mode to avoid unnecessary jumps.
pub fn buffer_bar_scroll(
    widths: &[u16],
    focused_index: Option<usize>,
    bar_width: u16,
    prev_scroll: u16,
    mode: BarScrollMode,
) -> u16 {
    let total: u16 = widths
        .iter()
        .copied()
        .fold(0u16, |a, w| a.saturating_add(w));
    if total <= bar_width || bar_width == 0 {
        return 0;
    }
    let max_scroll = total.saturating_sub(bar_width);

    let Some(idx) = focused_index else {
        return prev_scroll.min(max_scroll);
    };

    if idx >= widths.len() {
        // Multi-row theme: focused tab not on the first row; no scroll.
        return 0;
    }

    let tab_start: u16 = widths[..idx]
        .iter()
        .copied()
        .fold(0u16, |a, w| a.saturating_add(w));
    let tab_width = widths[idx];
    let tab_end = tab_start.saturating_add(tab_width);

    match mode {
        BarScrollMode::Center => {
            let offset = tab_start.saturating_sub(bar_width.saturating_sub(tab_width) / 2);
            offset.min(max_scroll)
        }
        BarScrollMode::Follow => {
            let mut scroll = prev_scroll;
            if tab_start < scroll {
                scroll = tab_start;
            } else if tab_end > scroll.saturating_add(bar_width) {
                scroll = tab_end.saturating_sub(bar_width);
            }
            scroll.min(max_scroll)
        }
    }
}

/// Upper bound on the terminal-cell height of an inline image, so a tall image
/// cannot push everything else off-screen. Width is bounded by the message area.
const MAX_IMAGE_ROWS: u16 = 12;

/// Upper bound on how many link previews are fetched/rendered per message, so a
/// message pasting many URLs cannot spawn unbounded fetches or dominate layout.
const MAX_PREVIEWS_PER_MESSAGE: usize = 2;

/// Upper bound on the cell-height of a link-preview thumbnail. Smaller than
/// [`MAX_IMAGE_ROWS`] since a preview is supplementary, not the message itself.
const MAX_PREVIEW_ROWS: u16 = 8;

/// One inline image to draw after the message list is rendered: the encoded
/// protocol is looked up by `path`, drawn into `rect`.
struct ImageDraw {
    path: PathBuf,
    rect: Rect,
}

/// An image attachment as needed by the renderer: its cache path (when the
/// backend downloaded it), plus name/url for the textual fallback shown when the
/// image cannot be rendered inline.
#[derive(Debug, Clone)]
struct ImageAttachment {
    path: Option<PathBuf>,
    name: String,
    url: Option<String>,
}

/// A request to decode and encode one image off the main loop. Sent by the
/// renderer (during draw) to the background decode worker; `avail` is the cell
/// area the protocol must fit into.
#[derive(Debug, Clone)]
pub struct DecodeRequest {
    pub path: PathBuf,
    pub avail: Size,
}

/// The result of a background decode, handed back to the main loop and then into
/// the renderer's cache. `protocol` is `None` when the file could not be
/// decoded/encoded, so the renderer can stop re-requesting it.
pub struct DecodedImage {
    pub path: PathBuf,
    pub protocol: Option<EncodedImage>,
}

/// An encoded image ready to draw.
pub enum EncodedImage {
    /// Drawn through ratatui-image's stateless widget as-is. Used outside tmux,
    /// and for kitty inside tmux (its unicode placeholders are plain text that
    /// tmux positions correctly in any pane).
    Widget(Protocol),
    /// Raw iTerm2/Sixel escape data for tmux, kept unwrapped but with every ESC
    /// already doubled for passthrough. Wrapped at draw time in a single
    /// passthrough that moves the outer terminal's cursor to the pane-absolute
    /// cell first, so the image lands in this pane even while another pane is
    /// active.
    TmuxRaw { data_doubled: String, size: Size },
}

impl EncodedImage {
    fn size(&self) -> Size {
        match self {
            EncodedImage::Widget(protocol) => protocol.size(),
            EncodedImage::TmuxRaw { size, .. } => *size,
        }
    }
}

/// Writes a raw graphics escape into the buffer the way ratatui-image's
/// stateless protocols do: the whole sequence lives in the area's first cell
/// (forced to width 1), and every other cell of the image area is marked skip
/// so the diff never paints text over the pixels. Skipped when the image does
/// not fit `rect`, matching the widget's behavior.
fn draw_raw_image(buf: &mut ratatui::buffer::Buffer, symbol: &str, size: Size, rect: Rect) {
    if size.width > rect.width || size.height > rect.height || size.width == 0 || size.height == 0 {
        return;
    }
    let area = Rect::new(rect.x, rect.y, size.width, size.height);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if (x, y) == (area.left(), area.top()) {
                continue;
            }
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_diff_option(ratatui::buffer::CellDiffOption::Skip);
            }
        }
    }
    if let Some(cell) = buf.cell_mut((area.left(), area.top())) {
        cell.set_symbol(symbol)
            .set_diff_option(ratatui::buffer::CellDiffOption::ForcedWidth(
                std::num::NonZeroU16::new(1).expect("1 is non-zero"),
            ));
    }
}

pub struct Renderer {
    /// Whether inline images can be rendered at all. `false` when graphics
    /// detection failed or the terminal has no graphics protocol; then images
    /// only show their textual fallback and no decodes are requested.
    images_enabled: bool,
    /// Channel to the background decode worker. `None` until wired at startup (or
    /// permanently when images are disabled); without it, images fall back to text.
    decode_tx: Option<UnboundedSender<DecodeRequest>>,
    /// Paths with a decode request already in flight, so the renderer does not
    /// re-send the same request on every frame while the worker is busy.
    pending: RefCell<HashSet<PathBuf>>,
    /// Paths whose decode failed; shown as the textual fallback permanently rather
    /// than re-requested each frame.
    failed: RefCell<HashSet<PathBuf>>,
    /// Encoded image protocols keyed by their cache-file path, so an image is
    /// decoded and encoded once and re-emitted cheaply each frame. Stateless
    /// protocols are used (rather than the diffing stateful ones) because the
    /// message list fully repaints every frame and its content scrolls, so an
    /// image must be re-placed from scratch each render. Behind a `RefCell`
    /// because rendered messages borrow `&self` while the cache is populated.
    image_cache: RefCell<HashMap<PathBuf, EncodedImage>>,
    /// Channel to the background link-preview worker. `None` until wired at
    /// startup (or permanently when link previews are disabled in config).
    preview_tx: Option<UnboundedSender<PreviewRequest>>,
    /// URLs with a preview fetch already in flight, so the renderer does not
    /// re-send the same request every frame while the worker is busy.
    preview_pending: RefCell<HashSet<String>>,
    /// URLs whose preview fetch found nothing usable (or failed); never retried.
    preview_failed: RefCell<HashSet<String>>,
    /// Fetched previews keyed by their source URL, so a URL is fetched once and
    /// re-rendered cheaply each frame. Its thumbnail (`image_path`) flows through
    /// the same `image_cache` pipeline as any other inline image.
    preview_cache: RefCell<HashMap<String, LinkPreview>>,
    /// Whether the terminal/pane currently has focus. Only used as a fallback:
    /// when the tmux pane origin is unknown, [`EncodedImage::TmuxRaw`] escapes
    /// would land at the active pane's cursor, so they are suppressed while
    /// unfocused. Assumed focused until a focus event says otherwise.
    focused: bool,
    /// Where this pane's top-left cell sits in the outer terminal, when running
    /// inside tmux with a cursor-positioned graphics protocol (iTerm2/Sixel).
    /// Refreshed on focus/resize/tick; `None` outside tmux or when the query
    /// failed.
    pane_origin: Option<PaneOrigin>,
    /// Whether quick reactions are offered on the selected message. When `false`
    /// the selected-message highlight and pill bar are not drawn. Set from config.
    quick_reactions_enabled: bool,
    /// Ordered emoji offered as quick reactions, bound to the number keys in
    /// select mode and drawn as the pill bar. Set from config.
    quick_reaction_emojis: Vec<String>,
}

/// What [`Renderer::render_messages`] hands back to `render`: the reaction-pill
/// hit boxes (screen `Rect` paired with the reaction it toggles), the inline
/// images to draw over the list, and the per-message row spans (paired with the
/// message's index-from-newest) for click-to-select.
type MessagesRender = (Vec<(Rect, ReactionHit)>, Vec<ImageDraw>, Vec<(Rect, usize)>);

#[derive(Debug, Clone, Default)]
pub struct RenderedMessage<'a> {
    pub time: Box<[Span<'a>]>,
    pub message: Box<Line<'a>>,
    /// Server event id of the message, when confirmed. `None` messages have no
    /// clickable reactions (there is nothing to toggle against yet).
    pub event_id: Option<EventId>,
    /// Reaction pills to draw on a dedicated row below the message, left to right
    /// in the order the theme returned them. For the selected message this also
    /// includes the quick-reaction pills, appended after the existing reactions so
    /// both share one unified row and one set of hit boxes.
    pub reactions: Vec<ReactionPill<'a>>,
    /// Image attachments to render inline (or as a text fallback) below the message.
    images: Vec<ImageAttachment>,
    /// Link-preview thumbnails to render inline below the message. Unlike
    /// `images`, these have no textual fallback: the preview's title/description
    /// lines (`preview_lines`) already stand in when the thumbnail can't render.
    preview_images: Vec<ImageAttachment>,
    /// Preformatted link-preview text rows (title/description/site), already
    /// styled by the `link_preview` theme formatter. Indented and appended below
    /// the message body during layout.
    preview_lines: Vec<Line<'a>>,
}

/// One reaction pill: its emoji key, the styled spans to draw, and the measured
/// display width used to place the pill's hit box.
#[derive(Debug, Clone)]
pub struct ReactionPill<'a> {
    pub key: String,
    pub spans: Vec<Span<'a>>,
    pub width: u16,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

/// A table is a styled span `{ value, style }` iff its second element is a table
/// tagged by `theme.style` (identity, not shape). Removes the old fragile
/// "length 2 and `from_value` happens to succeed" heuristic.
fn is_style_table(table: &mlua::Table) -> bool {
    table
        .metatable()
        .and_then(|mt| mt.get::<Option<bool>>(STYLE_MARKER).ok().flatten())
        .unwrap_or(false)
}

/// A message's image attachments, in order, with the data the renderer needs to
/// show each inline or (failing that) as a text fallback. Empty for non-message
/// events.
fn image_attachments(message: &StoredMessage) -> Vec<ImageAttachment> {
    let ChatEvent::Message { body, .. } = &message.event else {
        return Vec::new();
    };
    body.attachments
        .iter()
        .filter(|attachment| attachment.kind == AttachmentKind::Image)
        .map(|attachment| ImageAttachment {
            path: attachment.local_path.clone(),
            name: attachment.name.clone(),
            url: attachment.url.clone(),
        })
        .collect()
}

/// The http(s) URLs in a message's body worth previewing, capped at
/// [`MAX_PREVIEWS_PER_MESSAGE`]. Empty for non-message events.
fn message_urls(message: &StoredMessage) -> Vec<String> {
    let ChatEvent::Message { body, .. } = &message.event else {
        return Vec::new();
    };
    let mut urls = extract_urls(&body.text);
    urls.truncate(MAX_PREVIEWS_PER_MESSAGE);
    urls
}

/// Formats one captured log record as a styled line for the debug pane:
/// `HH:MM:SS LEVEL target: message`, colored by severity.
fn debug_log_line(line: &LogLine) -> Line<'static> {
    let level_style = match line.level {
        Level::ERROR => Style::default().fg(Color::Red),
        Level::WARN => Style::default().fg(Color::Yellow),
        Level::INFO => Style::default(),
        _ => Style::default().fg(Color::DarkGray),
    };
    Line::from(vec![
        Span::styled(
            line.time.format("%H:%M:%S ").to_string(),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(format!("{:<5} ", line.level), level_style),
        Span::styled(
            format!("{}: ", line.target),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(line.message.clone(), level_style),
    ])
}

/// A `[image: name] url` fallback line, shown for an image that could not be
/// rendered inline. Indented to align under the message body.
fn image_fallback_line(indent: u16, image: &ImageAttachment) -> Line<'static> {
    let mut spans = vec![
        Span::raw(" ".repeat(indent as usize)),
        Span::styled(
            format!("[image: {}]", image.name),
            Style::default().fg(Color::Blue),
        ),
    ];
    if let Some(url) = &image.url {
        spans.push(Span::styled(
            format!(" {url}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

/// Builds a trailing pill row (reactions or quick reactions), indented under the
/// message body with a single blank column between pills.
fn pill_row<'a>(indent_width: u16, pills: &[ReactionPill<'a>]) -> Line<'a> {
    let mut spans: Vec<Span<'a>> = vec![Span::raw(" ".repeat(indent_width as usize))];
    for (i, pill) in pills.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        spans.extend(pill.spans.iter().cloned());
    }
    Line::from(spans)
}

/// Records the per-pill hit boxes for one pill row at screen row `row_y`, mirroring
/// how the row is laid out by [`pill_row`] (indent, one blank column between
/// pills). Pills clipped past the right edge are truncated; ones fully off-screen
/// are skipped.
fn record_pill_hits(
    hits: &mut Vec<(Rect, ReactionHit)>,
    pills: &[ReactionPill<'_>],
    event_id: &EventId,
    list_area: Rect,
    indent_width: u16,
    row_y: u16,
) {
    let mut pill_x = list_area.x.saturating_add(indent_width);
    for (i, pill) in pills.iter().enumerate() {
        if i > 0 {
            pill_x = pill_x.saturating_add(1);
        }
        if pill_x < list_area.right() {
            let width = pill.width.min(list_area.right() - pill_x);
            hits.push((
                Rect {
                    x: pill_x,
                    y: row_y,
                    width,
                    height: 1,
                },
                ReactionHit {
                    event_id: event_id.clone(),
                    key: pill.key.clone(),
                },
            ));
        }
        pill_x = pill_x.saturating_add(pill.width);
    }
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            images_enabled: false,
            decode_tx: None,
            pending: RefCell::new(HashSet::new()),
            failed: RefCell::new(HashSet::new()),
            image_cache: RefCell::new(HashMap::new()),
            preview_tx: None,
            preview_pending: RefCell::new(HashSet::new()),
            preview_failed: RefCell::new(HashSet::new()),
            preview_cache: RefCell::new(HashMap::new()),
            focused: true,
            pane_origin: None,
            quick_reactions_enabled: true,
            quick_reaction_emojis: Vec::new(),
        }
    }

    /// Enables inline image rendering. Called once after the terminal is
    /// initialized when graphics support was detected.
    pub fn enable_images(&mut self) {
        self.images_enabled = true;
    }

    /// Configures the quick-reaction affordance (feature toggle + emoji set) from
    /// the user config. Called once at startup.
    pub fn set_quick_reactions(&mut self, config: &crate::config::QuickReactions) {
        self.quick_reactions_enabled = config.enabled;
        self.quick_reaction_emojis = config.emojis.clone();
    }

    /// Wires the channel used to request background image decodes. Called once at
    /// startup after the decode worker is spawned.
    pub fn set_decode_sender(&mut self, tx: UnboundedSender<DecodeRequest>) {
        self.decode_tx = Some(tx);
    }

    /// Consumes a finished background decode: caches the encoded protocol so the
    /// next frame draws it inline, or records the failure so it is not requested
    /// again. Either way the path is no longer in flight.
    pub fn insert_decoded(&mut self, decoded: DecodedImage) {
        self.pending.borrow_mut().remove(&decoded.path);
        match decoded.protocol {
            Some(protocol) => {
                self.image_cache.borrow_mut().insert(decoded.path, protocol);
            }
            None => {
                self.failed.borrow_mut().insert(decoded.path);
            }
        }
    }

    /// Wires the channel used to request background link-preview fetches. Called
    /// once at startup after the preview worker is spawned (only when link
    /// previews are enabled in config).
    pub fn set_preview_sender(&mut self, tx: UnboundedSender<PreviewRequest>) {
        self.preview_tx = Some(tx);
    }

    /// Consumes a finished preview fetch: caches a usable preview so the next
    /// frame renders it, or records the failure so it is not requested again.
    /// Either way the URL is no longer in flight.
    pub fn insert_link_preview(&mut self, result: PreviewResult) {
        self.preview_pending.borrow_mut().remove(&result.url);
        match result.preview {
            Some(preview) => {
                self.preview_cache.borrow_mut().insert(result.url, preview);
            }
            None => {
                self.preview_failed.borrow_mut().insert(result.url);
            }
        }
    }

    /// Queues a background preview fetch for `url`, unless one is already in
    /// flight or the URL already failed. Non-blocking; the worker sends the
    /// result back for the main loop to hand to [`Self::insert_link_preview`].
    fn request_preview(&self, url: &str) {
        let Some(tx) = &self.preview_tx else {
            return;
        };
        if self.preview_failed.borrow().contains(url) {
            return;
        }
        if !self.preview_pending.borrow_mut().insert(url.to_string()) {
            return;
        }
        let _ = tx.send(PreviewRequest {
            url: url.to_string(),
        });
    }

    /// Records terminal focus. Only consulted as a fallback: without a known
    /// tmux pane origin, [`EncodedImage::TmuxRaw`] escapes are not drawn while
    /// unfocused so they cannot leak into another (active) tmux pane.
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// Records where this pane sits in the outer terminal (or `None` when the
    /// tmux query failed). Re-queried on focus, resize, and tick events.
    /// Returns `true` when the origin changed, so the caller can repaint.
    pub fn set_pane_origin(&mut self, origin: Option<PaneOrigin>) -> bool {
        let changed = self.pane_origin != origin;
        self.pane_origin = origin;
        changed
    }

    /// Whether any decoded images are cached, i.e. whether keeping the tmux
    /// pane origin fresh is currently worth a subprocess per tick.
    pub fn has_cached_images(&self) -> bool {
        !self.image_cache.borrow().is_empty()
    }

    fn get_layout(&self, bar_height: u16) -> Layout {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(2),
                Constraint::Length(bar_height),
            ])
    }

    fn lua_value_to_spans(
        &self,
        lua: &mlua::Lua,
        value: mlua::Value,
    ) -> Result<Vec<Span<'_>>, anyhow::Error> {
        let mut spans = vec![];
        Self::flatten_lua_value(lua, value, &mut spans, None)?;
        Ok(spans)
    }

    fn flatten_lua_value(
        lua: &mlua::Lua,
        value: mlua::Value,
        spans: &mut Vec<Span>,
        parent_style: Option<Style>,
    ) -> mlua::Result<()> {
        match value {
            mlua::Value::String(str) => {
                let string = str.to_str()?.to_owned();
                spans.push(Self::string_to_span(string, parent_style));
            }
            mlua::Value::Table(v) => {
                if v.len()? == 2 {
                    if let mlua::Value::Table(style_table) = v.get::<mlua::Value>(2)? {
                        if is_style_table(&style_table) {
                            let style = lua
                                .from_value::<Style>(mlua::Value::Table(style_table))
                                .map(|style| match parent_style {
                                    Some(parent) => parent.patch(style),
                                    None => style,
                                })
                                .ok();

                            if let Some(style) = style {
                                if let Some(value) = v.get::<Option<mlua::Value>>(1)? {
                                    Self::flatten_lua_value(lua, value, spans, Some(style))?;
                                }
                                return Ok(());
                            }
                        }
                    }
                }

                for v in v.sequence_values::<mlua::Value>() {
                    Self::flatten_lua_value(lua, v?, spans, parent_style)?;
                }
            }
            _ => {}
        }

        Ok(())
    }

    fn string_to_span<'a>(str: String, style: Option<Style>) -> Span<'a> {
        match style {
            Some(style) => Span::styled(str, style),
            None => Span::raw(str),
        }
    }

    /// Calls the named UI formatter and converts its result into spans. A missing
    /// formatter yields no spans; a formatter that raises renders as a red
    /// `ERR: ...` span instead of crashing the renderer.
    fn format_spans<Args>(
        &self,
        lua: &mlua::Lua,
        name: &str,
        args: Args,
    ) -> Result<Vec<Span<'_>>, anyhow::Error>
    where
        Args: mlua::IntoLuaMulti,
    {
        match crate::config::call_formatter(lua, name, args) {
            None => Ok(vec![]),
            Some(Ok(value)) => self.lua_value_to_spans(lua, value),
            Some(Err(err)) => Ok(vec![Self::string_to_span(
                format!("ERR: {err}"),
                Some(Style::default().fg(Color::Red)),
            )]),
        }
    }

    /// Collects the ready link previews for a message: the thumbnails to draw
    /// inline and the styled title/description rows to append. URLs not yet
    /// fetched are queued (via [`Self::request_preview`]) and skipped this frame;
    /// they appear once their result lands and the frame repaints.
    fn build_previews(
        &self,
        lua: &mlua::Lua,
        message: &StoredMessage,
    ) -> (Vec<ImageAttachment>, Vec<Line<'_>>) {
        let mut images = Vec::new();
        let mut lines = Vec::new();
        for url in message_urls(message) {
            let cached = self.preview_cache.borrow().get(&url).cloned();
            let Some(preview) = cached else {
                self.request_preview(&url);
                continue;
            };
            if let Some(path) = &preview.image_path {
                images.push(ImageAttachment {
                    path: Some(path.clone()),
                    name: preview.title.clone().unwrap_or_default(),
                    url: Some(url.clone()),
                });
            }
            if let Ok(table) = self.preview_table(lua, &url, &preview) {
                lines.extend(self.format_preview_lines(lua, table));
            }
        }
        (images, lines)
    }

    /// Builds the Lua table handed to the `link_preview` formatter for one
    /// preview: `{ url, title?, description?, site_name? }`.
    fn preview_table(
        &self,
        lua: &mlua::Lua,
        url: &str,
        preview: &LinkPreview,
    ) -> mlua::Result<mlua::Table> {
        let table = lua.create_table()?;
        table.set("url", url)?;
        if let Some(title) = &preview.title {
            table.set("title", title.as_str())?;
        }
        if let Some(description) = &preview.description {
            // Passed verbatim: the theme owns display truncation/decoration.
            table.set("description", description.as_str())?;
        }
        if let Some(site_name) = &preview.site_name {
            table.set("site_name", site_name.as_str())?;
        }
        Ok(table)
    }

    /// Calls the `link_preview` formatter and converts its result - an array of
    /// rows, each row an array of spans - into styled [`Line`]s. A missing
    /// formatter or non-table result yields no rows.
    fn format_preview_lines(&self, lua: &mlua::Lua, preview: mlua::Table) -> Vec<Line<'_>> {
        let value = match crate::config::call_formatter(lua, "link_preview", preview) {
            Some(Ok(value)) => value,
            _ => return Vec::new(),
        };
        let mlua::Value::Table(rows) = value else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        for row in rows.sequence_values::<mlua::Value>() {
            let Ok(row) = row else { continue };
            if let Ok(spans) = self.lua_value_to_spans(lua, row) {
                if !spans.is_empty() {
                    lines.push(Line::from(spans));
                }
            }
        }
        lines
    }

    fn render_buffer_title(
        &self,
        lua: &mlua::Lua,
        backend: &BackendInfo,
        nickname: &str,
        buffer_label: &str,
    ) -> Result<Vec<Span<'_>>, anyhow::Error> {
        self.format_spans(
            lua,
            "buffer_title",
            (
                backend.name.clone(),
                nickname.to_string(),
                buffer_label.to_string(),
            ),
        )
    }

    /// Returns the cell size a decoded image occupies, or `None` when it is not
    /// ready to draw inline this frame (graphics disabled, decode failed, or still
    /// being decoded in the background). A miss - or a cached image that no longer
    /// fits `avail` because the terminal narrowed - triggers a background decode
    /// via [`Self::request_decode`]; the textual fallback shows until it lands.
    fn image_size(&self, path: &Path, avail: Size) -> Option<Size> {
        if !self.images_enabled || self.failed.borrow().contains(path) {
            return None;
        }
        if let Some(size) = self.image_cache.borrow().get(path).map(EncodedImage::size) {
            // Growth keeps the smaller size (fine); only a shrink below the fitted
            // size needs a re-encode, which happens off-thread.
            if size.width <= avail.width && size.height <= avail.height {
                return Some(size);
            }
        }
        self.request_decode(path, avail);
        None
    }

    /// Queues a background decode+encode for `path` at `avail`, unless one is
    /// already in flight. Non-blocking: the worker sends the result back and the
    /// main loop hands it to [`Self::insert_decoded`].
    fn request_decode(&self, path: &Path, avail: Size) {
        let Some(tx) = &self.decode_tx else {
            return;
        };
        if !self.pending.borrow_mut().insert(path.to_path_buf()) {
            return;
        }
        let _ = tx.send(DecodeRequest {
            path: path.to_path_buf(),
            avail,
        });
    }

    /// Draws the message list and returns the hit maps recorded this frame (see
    /// [`MessagesRender`]) plus the inline images to draw over the list, for the
    /// caller to place.
    fn render_messages(
        &self,
        f: &mut ratatui::Frame,
        state: &State,
        view: &ViewState,
        lua: &mlua::Lua,
        rect: Rect,
    ) -> MessagesRender {
        let Some((buffer_id, buffer, backend, nickname)) = self.focused(state, view) else {
            return (vec![], vec![], vec![]);
        };

        let target_name = buffer.label(&buffer_id.target);
        let total = buffer.messages.len();
        // Clamp scroll so we always render at least the oldest message when any exist.
        let scroll = buffer.scroll_position.min(total.saturating_sub(1));

        // Collect (RenderedMessage, message_time) pairs so we can detect the
        // read boundary and inject a separator between read and unread messages.
        let rendered: Vec<(RenderedMessage, chrono::DateTime<chrono::Local>, usize)> = buffer
            .messages
            .iter()
            .rev()
            // `enumerate` before `skip` so the index counts from the newest (0),
            // matching `view.selected_message` and the message-row hit map.
            .enumerate()
            .skip(scroll)
            // Render a bit more than fits, as some lines are filtered out and
            // others wrap.
            .take((rect.height as usize) + (rect.height as usize) / 2)
            .filter_map(|(index, message)| {
                // A reaction on this message is highlighted only if the hovered
                // pill belongs to it (matched by server event id).
                let hovered_key = view
                    .hovered_reaction
                    .as_ref()
                    .filter(|hit| message.event_id() == Some(&hit.event_id))
                    .map(|hit| hit.key.as_str());
                let selected = view.selected_message == Some(index);
                self.render_message(
                    lua,
                    backend,
                    &buffer_id.target,
                    target_name,
                    message,
                    hovered_key,
                    selected,
                )
                .map(|rm| (rm, message.time, index))
            })
            .collect();

        let title = self
            .render_buffer_title(lua, backend, nickname, buffer.label(&buffer_id.target))
            .unwrap_or_default();

        // Build the block up front so `list_area` can be derived from the exact
        // geometry `List` will render into (the title consumes one top row). This
        // lets the reaction hit boxes below mirror the widget's placement.
        let block = Block::default().title(title).borders(Borders::NONE);
        let list_area = block.inner(rect);

        let read_marker = buffer.read_marker;
        let mut seen_unread = false;
        let mut separator_inserted = false;
        let mut prev_date: Option<chrono::NaiveDate> = None;
        let mut prev_msg_time: Option<chrono::DateTime<chrono::Local>> = None;
        let mut messages: Vec<ListItem<'_>> = Vec::with_capacity(rendered.len() + 2);
        let mut reaction_hits: Vec<(Rect, ReactionHit)> = vec![];
        let mut image_draws: Vec<ImageDraw> = vec![];
        // Per-message row spans paired with their index-from-newest, for
        // click-to-select. Only fully-visible messages are recorded.
        let mut message_rows: Vec<(Rect, usize)> = vec![];

        // Rows of every item pushed so far. `List` (BottomToTop) anchors item 0 at
        // the bottom, so item `i` occupies rows `[bottom - cum - h, bottom - cum)`
        // and is drawn iff `cum + h <= list_area.height` (no partial top clip).
        let mut cum: u16 = 0;

        for (rm, msg_time, msg_index) in &rendered {
            if rm.message.width() == 0 {
                continue;
            }

            if !separator_inserted {
                if let Some(marker) = read_marker {
                    if msg_time > &marker {
                        // Still in unread territory; remember we've seen at least one.
                        seen_unread = true;
                    } else if seen_unread {
                        // First read message after one or more unread ones: inject separator.
                        messages.push(ListItem::new(self.render_unread_separator(lua, rect.width)));
                        cum = cum.saturating_add(1);
                        separator_inserted = true;
                    }
                }
            }

            // Date separator: inject when the calendar day changes between messages.
            // We iterate newest-first, so when the date decreases we push a separator
            // labelled with the newer date (prev_msg_time), which visually appears at
            // the top of that day's block when the list is rendered bottom-to-top.
            let current_date = msg_time.date_naive();
            if let (Some(prev), Some(sep_time)) = (prev_date, prev_msg_time) {
                if current_date != prev {
                    messages.push(ListItem::new(
                        self.render_date_separator(lua, &sep_time, rect.width),
                    ));
                    cum = cum.saturating_add(1);
                }
            }
            prev_date = Some(current_date);
            prev_msg_time = Some(*msg_time);

            let initial_indent = rm.time.clone();
            // Column where the message body (and thus the reaction row) begins.
            let indent_width: u16 = initial_indent
                .iter()
                .map(|span| span.width())
                .sum::<usize>() as u16;

            let subsequent_indent = if !initial_indent.is_empty() {
                Box::new([
                    Span::raw(
                        " ".repeat(
                            initial_indent
                                .iter()
                                .take(initial_indent.len() - 1)
                                .map(|span| span.width())
                                .sum(),
                        ),
                    ),
                    initial_indent.iter().last().unwrap().clone(),
                ])
            } else {
                Box::new([Span::raw(""), Span::raw("")])
            };

            // Rows below the message's first line (wrapped preview text, and the
            // reserved rows behind inline attachment/preview images) are indented
            // like the message's wrapped continuation rows, so the timestamp
            // separator (last span of `subsequent_indent`, e.g. `▏`) continues
            // down through the whole item rather than leaving that column blank.
            // Cloned before `subsequent_indent` is consumed by the body wrap below.
            let continuation_indent = subsequent_indent.clone();

            let mut text = wrap_line(
                &rm.message,
                super::wrap::Options {
                    width: rect.width as usize,
                    initial_indent,
                    subsequent_indent,
                    break_words: true,
                },
            );
            // Inline images begin on the message's first row, just after its text
            // (`<nick> <image>`), and span downward. The image column is one past
            // the first line's content; its width is what remains to the right.
            let first_line_width = text
                .lines
                .first()
                .map(|line| line.width() as u16)
                .unwrap_or(0);
            let image_x = list_area
                .x
                .saturating_add(first_line_width)
                .saturating_add(1);
            let image_avail_width = list_area.right().saturating_sub(image_x);
            let avail = Size {
                width: image_avail_width,
                height: MAX_IMAGE_ROWS,
            };

            // Decide per image whether it renders inline: if so, it is drawn over
            // reserved blank rows (no text); otherwise a `[image: name] url`
            // fallback line is shown. So the label is never duplicated alongside
            // the picture.
            let mut inline_images: Vec<(PathBuf, Size)> = Vec::new();
            for image in &rm.images {
                let inline = if self.images_enabled && image_avail_width > 0 {
                    image
                        .path
                        .as_ref()
                        .and_then(|path| self.image_size(path, avail).map(|size| (path, size)))
                        .filter(|(_, size)| size.width > 0 && size.height > 0)
                } else {
                    None
                };
                match inline {
                    Some((path, size)) => inline_images.push((path.clone(), size)),
                    None => text.lines.push(image_fallback_line(indent_width, image)),
                }
            }
            // The message-attachment images form a vertical strip starting at the
            // first row. Reserve its rows now (before the preview block below) so
            // the preview sits under both the message text and any attachments.
            let images_total_h: u16 = inline_images.iter().map(|(_, size)| size.height).sum();
            let base_lines = text.lines.len() as u16;
            // Carry the base indent on the reserved rows so the timestamp
            // separator keeps running down beside the image. The image is drawn
            // from `image_x` (past the first line's text, so well right of the
            // `▏` in the indent), so it never covers the separator.
            for _ in base_lines..images_total_h {
                text.lines.push(Line::from(continuation_indent.to_vec()));
            }

            // Measure the link-preview block (title/description rows + thumbnail)
            // before committing it. The preview is supplementary: if appending it
            // would push the item past the remaining space - so `List` (which drops
            // any not-fully-visible item) would hide the message itself - we skip
            // the preview and still render the message.
            // The theme's per-row leading decoration (e.g. the `▎` gutter, each
            // preview row's first span); cloned onto the reserved thumbnail rows
            // so the gutter runs down beside the image too. The thumbnail shifts
            // right past it, aligning with the preview text content. Image-only
            // previews have no text rows and thus no gutter to continue.
            let preview_gutter: Option<Span<'_>> = rm
                .preview_lines
                .first()
                .and_then(|line| line.spans.first())
                .cloned();
            let preview_gutter_w: u16 = preview_gutter
                .as_ref()
                .map(|span| span.width() as u16)
                .unwrap_or(0);
            let preview_x = list_area
                .x
                .saturating_add(indent_width)
                .saturating_add(preview_gutter_w);
            let preview_avail_width = list_area.right().saturating_sub(preview_x);
            let preview_avail = Size {
                width: preview_avail_width,
                height: MAX_PREVIEW_ROWS,
            };
            let mut preview_thumbs: Vec<(PathBuf, Size)> = Vec::new();
            if self.images_enabled && preview_avail_width > 0 {
                for image in &rm.preview_images {
                    if let Some((path, size)) = image
                        .path
                        .as_ref()
                        .and_then(|path| {
                            self.image_size(path, preview_avail)
                                .map(|size| (path, size))
                        })
                        .filter(|(_, size)| size.width > 0 && size.height > 0)
                    {
                        preview_thumbs.push((path.clone(), size));
                    }
                }
            }
            let preview_thumbs_h: u16 = preview_thumbs.iter().map(|(_, size)| size.height).sum();

            // Wrap the preview title/description rows the same way the message
            // body is wrapped, so long previews break within the message area
            // instead of overflowing. Done before measuring so `preview_extra`
            // reflects the rows actually pushed below. The base indent
            // (`continuation_indent`) carries the message's timestamp separator,
            // so `▏` continues down every preview row; each row additionally
            // repeats its own leading decoration (the theme's `▎` gutter, its
            // first span), so both bars run unbroken down the whole preview.
            let wrapped_preview_lines: Vec<Line<'_>> = rm
                .preview_lines
                .iter()
                .flat_map(|line| {
                    let mut subsequent: Vec<Span<'_>> = continuation_indent.to_vec();
                    if let Some(gutter) = line.spans.first() {
                        subsequent.push(gutter.clone());
                    }
                    wrap_line(
                        line,
                        super::wrap::Options {
                            width: rect.width as usize,
                            initial_indent: continuation_indent.clone(),
                            subsequent_indent: subsequent.into_boxed_slice(),
                            break_words: true,
                        },
                    )
                    .lines
                })
                .collect();
            let preview_extra = wrapped_preview_lines.len() as u16 + preview_thumbs_h;

            // Core height is the message (already in `text.lines`) plus the reaction
            // row appended below. Include the preview only if the whole item still
            // fully fits the remaining space.
            let reaction_rows: u16 = if rm.reactions.is_empty() { 0 } else { 1 };
            let core_h = text.lines.len() as u16 + reaction_rows;
            let include_preview = preview_extra > 0
                && cum.saturating_add(core_h.saturating_add(preview_extra)) <= list_area.height;

            // Row (relative to the item's top) where the thumbnail strip begins;
            // only meaningful when the preview is included.
            let mut preview_thumb_row = 0u16;
            if include_preview {
                // Title/description rows on new lines below the message body, then
                // the thumbnail on its own reserved rows beneath them.
                for line in &wrapped_preview_lines {
                    text.lines.push(line.clone());
                }
                preview_thumb_row = text.lines.len() as u16;
                // Reserve the thumbnail rows carrying the base indent plus the
                // preview gutter, so both the timestamp separator and the `▎`
                // gutter keep running down beside the image. The thumbnail is
                // drawn from `preview_x` (past `indent_width` and the gutter)
                // rightward, so it covers neither bar.
                for _ in 0..preview_thumbs_h {
                    let mut spans = continuation_indent.to_vec();
                    spans.extend(preview_gutter.clone());
                    text.lines.push(Line::from(spans));
                }
            } else {
                // Dropped for lack of space: draw no thumbnail for this item.
                preview_thumbs.clear();
            }

            // Append the reaction pills on their own trailing row, aligned under
            // the message body. Kept last so its hit-box geometry is unchanged by
            // the image rows above it. Pills are separated by a single blank column.
            // For the selected message this row already includes the quick-reaction
            // pills (appended in `render_message`), so both share one row.
            if !rm.reactions.is_empty() {
                text.lines.push(pill_row(indent_width, &rm.reactions));
            }

            let h = text.lines.len() as u16;
            // The item is drawn only when it fully fits (List drops overflow
            // items); its hit boxes and images must obey the same rule.
            let fully_visible = cum.saturating_add(h) <= list_area.height;

            // Record hit boxes for the reaction row (the item's last row), only
            // when fully visible and the message has a server id to toggle.
            if let Some(event_id) = &rm.event_id {
                if !rm.reactions.is_empty() && fully_visible {
                    let row_y = list_area.bottom().saturating_sub(1).saturating_sub(cum);
                    record_pill_hits(
                        &mut reaction_hits,
                        &rm.reactions,
                        event_id,
                        list_area,
                        indent_width,
                        row_y,
                    );
                }
            }

            // Record the message's full row span for click-to-select. Its top row
            // is `bottom - cum - h`; pill/reaction clicks are resolved before this
            // map, so spanning the whole item is safe.
            if fully_visible {
                let item_top = list_area.bottom().saturating_sub(cum).saturating_sub(h);
                message_rows.push((
                    Rect {
                        x: list_area.x,
                        y: item_top,
                        width: list_area.width,
                        height: h,
                    },
                    *msg_index,
                ));
            }

            // Record the image rectangles. The strip begins at the item's top row
            // (`bottom - cum - h`), just right of the first line's text, and each
            // image stacks below the previous.
            if fully_visible && !inline_images.is_empty() && image_x < list_area.right() {
                let item_top = list_area.bottom().saturating_sub(cum).saturating_sub(h);
                let mut y = item_top;
                for (path, size) in inline_images {
                    let width = size.width.min(list_area.right() - image_x);
                    image_draws.push(ImageDraw {
                        path,
                        rect: Rect {
                            x: image_x,
                            y,
                            width,
                            height: size.height,
                        },
                    });
                    y = y.saturating_add(size.height);
                }
            }

            // Record the preview thumbnail rectangles: their own vertical strip on
            // the rows reserved below the message/preview text, left-aligned under
            // the message body (`item_top + preview_thumb_row`).
            if fully_visible && !preview_thumbs.is_empty() && preview_x < list_area.right() {
                let item_top = list_area.bottom().saturating_sub(cum).saturating_sub(h);
                let mut y = item_top.saturating_add(preview_thumb_row);
                for (path, size) in preview_thumbs {
                    let width = size.width.min(list_area.right() - preview_x);
                    image_draws.push(ImageDraw {
                        path,
                        rect: Rect {
                            x: preview_x,
                            y,
                            width,
                            height: size.height,
                        },
                    });
                    y = y.saturating_add(size.height);
                }
            }

            cum = cum.saturating_add(h);
            messages.push(ListItem::new(text));
        }

        // The loop only emits a separator when the day changes between two
        // messages, so the oldest day's block has none at its top. Push a final
        // separator labelled with the oldest message's date; rendered
        // bottom-to-top it lands above the first message. (Dropped by `List` when
        // the oldest message is scrolled out of view.)
        if let Some(sep_time) = prev_msg_time {
            messages.push(ListItem::new(
                self.render_date_separator(lua, &sep_time, rect.width),
            ));
        }

        let list = List::new(messages)
            .block(block)
            .direction(ListDirection::BottomToTop);

        f.render_widget(list, rect);

        (reaction_hits, image_draws, message_rows)
    }

    /// Draws the inline images recorded by [`Self::render_messages`] over the
    /// list, each into its reserved rectangle. Runs after the list so the image
    /// protocol's cells overwrite the blank rows.
    fn draw_images(&self, f: &mut ratatui::Frame, draws: Vec<ImageDraw>) {
        let cache = self.image_cache.borrow();
        for draw in draws {
            match cache.get(&draw.path) {
                Some(EncodedImage::Widget(protocol)) => {
                    f.render_widget(Image::new(protocol), draw.rect);
                }
                Some(EncodedImage::TmuxRaw { data_doubled, size }) => {
                    let symbol = match self.pane_origin {
                        // Position the outer cursor at this pane's absolute
                        // cell inside the passthrough, so the image lands here
                        // even while another tmux pane is active. The absolute
                        // coordinates are part of the cell symbol, so a layout
                        // change re-emits the image via the normal cell diff.
                        Some(origin) => wrap_passthrough_positioned(
                            data_doubled,
                            origin.row + draw.rect.y,
                            origin.col + draw.rect.x,
                        ),
                        // Origin unknown: the escape draws at the active
                        // pane's cursor, which is only ours while focused.
                        None if self.focused => wrap_passthrough(data_doubled),
                        None => continue,
                    };
                    draw_raw_image(f.buffer_mut(), &symbol, *size, draw.rect);
                }
                None => {}
            }
        }
    }

    /// Renders the "new messages" separator line. The appearance is driven by
    /// the theme's `render_unread_separator` formatter; falls back to a plain
    /// styled line when the theme does not implement it.
    fn render_unread_separator(&self, lua: &mlua::Lua, width: u16) -> Line<'_> {
        match self.format_spans(lua, "render_unread_separator", width as usize) {
            Ok(spans) if !spans.is_empty() => Line::from(spans),
            _ => Line::from(Span::styled(
                "─── new messages ───",
                Style::default().fg(Color::DarkGray),
            )),
        }
    }

    /// Renders the date-change separator. `date` is the newer day's representative
    /// timestamp (the first message of that day in chronological order, i.e. the
    /// messages rendered below the separator). Appearance is driven by the theme's
    /// `render_date_separator` formatter; falls back to a plain `Month D, YYYY` line.
    fn render_date_separator(
        &self,
        lua: &mlua::Lua,
        date: &chrono::DateTime<chrono::Local>,
        width: u16,
    ) -> Line<'_> {
        let fallback = date.format("─── %-d %B %Y ───").to_string();
        match date_time_to_table(lua, date).ok().and_then(|dt| {
            self.format_spans(lua, "render_date_separator", (dt, width as usize))
                .ok()
        }) {
            Some(spans) if !spans.is_empty() => Line::from(spans),
            _ => Line::from(Span::styled(fallback, Style::default().fg(Color::DarkGray))),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_message(
        &self,
        lua: &mlua::Lua,
        backend: &BackendInfo,
        target: &TargetId,
        target_name: &str,
        message: &StoredMessage,
        hovered_key: Option<&str>,
        selected: bool,
    ) -> Option<RenderedMessage<'_>> {
        let event = to_lua_event(lua, message, backend, target, target_name).ok()?;

        let mut time_spans = date_time_to_table(lua, &message.time)
            .ok()
            .and_then(|date_time| {
                self.format_spans(lua, "message_time", (date_time, &event))
                    .ok()
            })
            .unwrap_or_default();

        if time_spans.len() == 1 {
            time_spans.push(Span::raw(""));
        }

        let message_spans = self
            .format_spans(lua, "message_text", (&event, backend.name.clone()))
            .unwrap_or_default();

        if message_spans.is_empty() {
            return None;
        }

        let mut reactions = self.render_reaction_pills(lua, &event, hovered_key);
        // For the selected message with a server id to react to, append the
        // quick-reaction pills onto the same row so existing reactions and quick
        // reactions read as one unified, uniformly styled strip.
        if selected && message.event_id().is_some() {
            reactions.extend(self.render_quick_reaction_pills(lua, &event, hovered_key));
        }
        let (preview_images, preview_lines) = self.build_previews(lua, message);

        Some(RenderedMessage {
            time: time_spans.into_boxed_slice(),
            message: Box::new(Line::from(message_spans)),
            event_id: message.event_id().cloned(),
            reactions,
            images: image_attachments(message),
            preview_images,
            preview_lines,
        })
    }

    /// Calls the theme's `render_reactions` formatter and converts each returned
    /// pill (`{ key, spans }`) into a measured [`ReactionPill`]. Pills with zero
    /// width are dropped so they never produce an unclickable hit box.
    fn render_reaction_pills(
        &self,
        lua: &mlua::Lua,
        event: &mlua::Table,
        hovered_key: Option<&str>,
    ) -> Vec<ReactionPill<'_>> {
        let value =
            match crate::config::call_formatter(lua, "render_reactions", (event, hovered_key)) {
                Some(Ok(value)) => value,
                _ => return vec![],
            };

        self.pills_from_lua_value(lua, value)
    }

    /// Calls the theme's `render_quick_reactions` formatter for the selected
    /// message, passing the configured emoji set and the hovered key (so quick
    /// pills get the same hover highlight as ordinary reactions), and converts the
    /// returned pills like [`Self::render_reaction_pills`]. Empty when quick
    /// reactions are disabled, no emoji are configured, or the theme has no
    /// formatter.
    fn render_quick_reaction_pills(
        &self,
        lua: &mlua::Lua,
        event: &mlua::Table,
        hovered_key: Option<&str>,
    ) -> Vec<ReactionPill<'_>> {
        if !self.quick_reactions_enabled || self.quick_reaction_emojis.is_empty() {
            return vec![];
        }
        let Ok(emojis) = lua.create_sequence_from(self.quick_reaction_emojis.iter().cloned())
        else {
            return vec![];
        };
        let value = match crate::config::call_formatter(
            lua,
            "render_quick_reactions",
            (event, emojis, hovered_key),
        ) {
            Some(Ok(value)) => value,
            _ => return vec![],
        };

        self.pills_from_lua_value(lua, value)
    }

    /// Parses a theme formatter's returned pill list (`{ { key, spans }, ... }`)
    /// into measured [`ReactionPill`]s. Zero-width pills are dropped so they never
    /// produce an unclickable hit box. Shared by the reaction and quick-reaction
    /// formatters, which return the same shape.
    fn pills_from_lua_value(&self, lua: &mlua::Lua, value: mlua::Value) -> Vec<ReactionPill<'_>> {
        let mlua::Value::Table(list) = value else {
            return vec![];
        };

        let mut pills = vec![];
        for entry in list.sequence_values::<mlua::Value>() {
            let Ok(mlua::Value::Table(pill)) = entry else {
                continue;
            };
            let Ok(key) = pill.get::<String>("key") else {
                continue;
            };
            let spans = pill
                .get::<mlua::Value>("spans")
                .ok()
                .and_then(|spans| self.lua_value_to_spans(lua, spans).ok())
                .unwrap_or_default();
            let width: u16 = spans.iter().map(|s| s.width()).sum::<usize>() as u16;
            if width == 0 {
                continue;
            }
            pills.push(ReactionPill { key, spans, width });
        }
        pills
    }

    /// Builds the `TircBufferTab` table for one buffer, the shape passed to the
    /// theme's `render_buffer_tab`/`render_buffer_bar` formatters.
    fn buffer_tab_table(
        &self,
        state: &State,
        lua: &mlua::Lua,
        id: &BufferId,
        buffer: &ChatBuffer,
    ) -> mlua::Result<mlua::Table> {
        let backend_name = state
            .backends
            .get(&id.backend)
            .map(|b| b.info.name.as_str())
            .unwrap_or("?");

        let t = lua.create_table()?;
        t.set("id", format!("{}:{}", id.backend.0, id.target.as_str()))?;
        t.set("name", buffer.label(&id.target))?;
        t.set("target", id.target.as_str())?;
        t.set("is_status", id.target.is_status())?;
        t.set(
            "is_system",
            matches!(buffer.kind, crate::core::BufferKind::System),
        )?;
        t.set("backend_id", id.backend.0)?;
        t.set("backend_name", backend_name)?;
        if let Some(metadata) = crate::config::get_backend_metadata(lua, id.backend) {
            t.set("backend_metadata", metadata)?;
        }
        t.set("has_unread", buffer.has_unread)?;
        t.set("has_mention", buffer.has_mention)?;
        if let Some(backend_state) = state.backends.get(&id.backend) {
            t.set("latency_ms", backend_state.latency_ms)?;
            t.set(
                "connection_status",
                match backend_state.connection_status {
                    ConnectionStatus::Connecting => "connecting",
                    ConnectionStatus::Connected => "connected",
                    ConnectionStatus::Disconnected => "disconnected",
                },
            )?;
        }
        Ok(t)
    }

    /// Builds the Lua array of all buffer tabs, in buffer order.
    fn buffer_tabs(&self, state: &State, lua: &mlua::Lua) -> mlua::Result<mlua::Table> {
        let tabs = lua.create_table()?;
        for (id, buffer) in state.buffers.iter() {
            tabs.push(self.buffer_tab_table(state, lua, id, buffer)?)?;
        }
        Ok(tabs)
    }

    /// Accumulates left-to-right hit boxes along the first row of the bar from
    /// the per-tab `widths` measured while the bar was built (see
    /// [`Self::build_buffer_bar`]), pairing each with its buffer in
    /// `state.buffers` order. Because the widths come from the same flatten that
    /// produced the drawn spans, the boxes line up with the bar exactly even when
    /// a tab includes separators. Any separator a tab carries is attributed to
    /// that tab, leaving the bar contiguous with no dead zones between tabs.
    fn bar_tabs_from_widths(
        &self,
        state: &State,
        bar_rect: Rect,
        widths: &[u16],
        scroll: u16,
    ) -> Vec<(Rect, BufferId)> {
        let mut tabs = Vec::with_capacity(widths.len());
        let mut content_x: u16 = 0;

        for (width, id) in widths.iter().zip(state.buffers.keys()) {
            let w = *width;
            let content_end = content_x.saturating_add(w);

            if w > 0 && content_end > scroll {
                let rel_start = content_x.saturating_sub(scroll);
                if rel_start < bar_rect.width {
                    let rel_end = content_end.saturating_sub(scroll).min(bar_rect.width);
                    let visible_width = rel_end.saturating_sub(rel_start);
                    if visible_width > 0 {
                        tabs.push((
                            Rect {
                                x: bar_rect.x.saturating_add(rel_start),
                                y: bar_rect.y,
                                width: visible_width,
                                height: 1,
                            },
                            id.clone(),
                        ));
                    }
                }
            }

            content_x = content_end;
        }

        tabs
    }

    fn update_render_context(
        &self,
        lua: &mlua::Lua,
        view: &ViewState,
        state: &State,
    ) -> anyhow::Result<()> {
        let tirc_mod: mlua::Table = lua
            .globals()
            .get::<mlua::Table>("package")?
            .get::<mlua::Table>("loaded")?
            .get::<mlua::Table>("_tirc")?;

        tirc_mod.set(
            "mode",
            match view.mode {
                Mode::Normal => "normal",
                Mode::Command => "command",
                Mode::Insert => "insert",
                Mode::Select => "select",
            },
        )?;
        tirc_mod.set("multi_backend", state.backends.len() > 1)?;
        tirc_mod.set("buffers", self.buffer_tabs(state, lua)?)?;

        match &view.focused {
            Some(id) => {
                let id_str = format!("{}:{}", id.backend.0, id.target.as_str());
                tirc_mod.set("focused_buffer", id_str)?;
            }
            None => tirc_mod.set("focused_buffer", mlua::Value::Nil)?,
        }

        Ok(())
    }

    /// Converts a `render_buffer_bar` result into rendered lines plus the display
    /// width of each top-level element of the *first* row. A table with a `rows`
    /// sequence yields one line per row; any other value is treated as a single
    /// row (the shorthand documented for `render_buffer_bar`).
    ///
    /// Each first-row element is one buffer tab, in buffer order (the
    /// `render_buffer_bar` contract), so these widths drive click hit-testing:
    /// measuring the same structure that is rendered keeps the hit boxes exact
    /// even for themes whose tabs include separators (e.g. `slanted`), which a
    /// separate per-tab re-measure could not match.
    fn rows_to_lines_and_widths(
        &self,
        lua: &mlua::Lua,
        value: mlua::Value,
    ) -> Result<(Vec<Line<'_>>, Vec<u16>), anyhow::Error> {
        if let mlua::Value::Table(table) = &value {
            if let mlua::Value::Table(rows) = table.get::<mlua::Value>("rows")? {
                let mut lines = Vec::new();
                let mut first_row_widths = Vec::new();
                for (i, row) in rows.sequence_values::<mlua::Value>().enumerate() {
                    let (line, widths) = self.row_line_and_widths(lua, row?)?;
                    if i == 0 {
                        first_row_widths = widths;
                    }
                    lines.push(line);
                }
                return Ok((lines, first_row_widths));
            }
        }

        let (line, widths) = self.row_line_and_widths(lua, value)?;
        Ok((vec![line], widths))
    }

    /// Flattens one bar row into a [`Line`] and returns the display width of each
    /// of the row's top-level elements (one per tab). A non-table row has no
    /// per-tab structure, so it yields a single line and empty widths.
    fn row_line_and_widths(
        &self,
        lua: &mlua::Lua,
        row: mlua::Value,
    ) -> Result<(Line<'_>, Vec<u16>), anyhow::Error> {
        match row {
            mlua::Value::Table(table) => {
                let mut spans = Vec::new();
                let mut widths = Vec::new();
                for element in table.sequence_values::<mlua::Value>() {
                    let element_spans = self.lua_value_to_spans(lua, element?)?;
                    widths.push(element_spans.iter().map(|s| s.width() as u16).sum());
                    spans.extend(element_spans);
                }
                Ok((Line::from(spans), widths))
            }
            other => Ok((Line::from(self.lua_value_to_spans(lua, other)?), Vec::new())),
        }
    }

    /// Extracts the optional `bg` colour from a `TircBufferBar` table, returning
    /// a `Style` with that background set, or the default style if absent/invalid.
    fn bar_bg_style(table: &mlua::Table) -> Style {
        table
            .get::<Option<String>>("bg")
            .ok()
            .flatten()
            .and_then(|s| std::str::FromStr::from_str(&s).ok())
            .map(|c: Color| Style::default().bg(c))
            .unwrap_or_default()
    }

    /// Extracts the optional `scroll` mode from a `TircBufferBar` table.
    /// `"center"` maps to `Center`; everything else (including absent) is `Follow`.
    fn bar_scroll_mode(table: &mlua::Table) -> BarScrollMode {
        match table.get::<Option<String>>("scroll").ok().flatten() {
            Some(s) if s == "center" => BarScrollMode::Center,
            _ => BarScrollMode::Follow,
        }
    }

    /// Produces the buffer bar as a list of lines, a base background style, and
    /// the per-tab column widths of the first row (in buffer order) used for
    /// click hit-testing. Delegates the whole layout to the theme's
    /// `render_buffer_bar`; when that formatter is absent, falls back to a single
    /// line built from per-tab `render_buffer_tab` results so raw `TircUi` themes
    /// keep working.
    fn build_buffer_bar(
        &self,
        state: &State,
        lua: &mlua::Lua,
    ) -> (Vec<Line<'_>>, Style, Vec<u16>, BarScrollMode) {
        let tabs = match self.buffer_tabs(state, lua) {
            Ok(tabs) => tabs,
            Err(_) => {
                return (
                    vec![Line::default()],
                    Style::default(),
                    Vec::new(),
                    BarScrollMode::default(),
                )
            }
        };

        match crate::config::call_formatter(lua, "render_buffer_bar", &tabs) {
            Some(Ok(mlua::Value::Table(table))) => {
                let bg_style = Self::bar_bg_style(&table);
                let scroll_mode = Self::bar_scroll_mode(&table);
                let (lines, widths) = self
                    .rows_to_lines_and_widths(lua, mlua::Value::Table(table))
                    .unwrap_or_default();
                (lines, bg_style, widths, scroll_mode)
            }
            Some(Ok(value)) => {
                let (lines, widths) = self
                    .rows_to_lines_and_widths(lua, value)
                    .unwrap_or_default();
                (lines, Style::default(), widths, BarScrollMode::default())
            }
            Some(Err(err)) => (
                vec![Line::from(Self::string_to_span(
                    format!("ERR: {err}"),
                    Some(Style::default().fg(Color::Red)),
                ))],
                Style::default(),
                Vec::new(),
                BarScrollMode::default(),
            ),
            None => {
                // No `render_buffer_bar`: build a single row from per-tab spans and
                // measure each tab so hit-testing still works.
                let mut spans = Vec::new();
                let mut widths = Vec::new();
                for tab in tabs.sequence_values::<mlua::Table>().filter_map(Result::ok) {
                    let tab_spans = self
                        .format_spans(lua, "render_buffer_tab", tab)
                        .unwrap_or_default();
                    widths.push(tab_spans.iter().map(|s| s.width() as u16).sum());
                    spans.extend(tab_spans);
                }
                (
                    vec![Line::from(spans)],
                    Style::default(),
                    widths,
                    BarScrollMode::default(),
                )
            }
        }
    }

    fn render_input(
        &mut self,
        f: &mut ratatui::Frame,
        view: &ViewState,
        input: &Input,
        can_post: bool,
        rect: Rect,
    ) {
        let prefix = match view.mode {
            Mode::Normal => "",
            Mode::Command => ":",
            Mode::Insert => "❯ ",
            Mode::Select => "",
        };
        let prefix_len = prefix.chars().count() as u16;
        let width = f.area().width.max(3) - prefix_len;
        let scroll = input.visual_scroll(width as usize);
        let p = Paragraph::new(format!("{}{}", prefix, input.value()))
            .scroll((0, scroll as u16))
            .block(Block::default().borders(Borders::TOP));
        f.render_widget(p, rect);

        // Surface active mode indicators (copy mode releases mouse capture for
        // native selection; the debug pane is open) as a right-aligned hint on the
        // input row. Drawn over the same rect after the input so it sits on the
        // text row (the block's top border is row `rect.y`).
        let mut hints: Vec<&str> = Vec::new();
        if view.mode == Mode::Select {
            hints.push("-- SELECT --");
        }
        if view.debug_open {
            hints.push("-- DEBUG --");
        }
        if view.copy_mode {
            hints.push("-- COPY --");
        }
        // Read-only room: hint that typed input will not be delivered. Shown while
        // composing (Insert mode), where the user would otherwise get no feedback
        // until a send silently fails.
        if !can_post && view.mode == Mode::Insert {
            hints.push("-- no permission to post --");
        }
        if !hints.is_empty() {
            let hint = Paragraph::new(Line::from(Span::styled(
                hints.join("  "),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )))
            .alignment(ratatui::layout::Alignment::Right)
            .block(Block::default().borders(Borders::TOP));
            f.render_widget(hint, rect);
        }

        match view.mode {
            Mode::Normal | Mode::Select => {}
            Mode::Command | Mode::Insert => f.set_cursor_position((
                rect.x + ((input.visual_cursor()).max(scroll) - scroll) as u16 + prefix_len,
                rect.y + 1,
            )),
        }
    }

    fn render_user(
        &self,
        lua: &mlua::Lua,
        user: &mlua::Table,
    ) -> Result<Vec<Span<'_>>, anyhow::Error> {
        self.format_spans(lua, "user", user)
    }

    fn render_users(
        &self,
        f: &mut ratatui::Frame,
        members: &[Member],
        lua: &mlua::Lua,
        title: &str,
        rect: Rect,
    ) {
        let users = members
            // Members are kept sorted by (role, name) in state, so render is a
            // pure read. TODO: make the user list scrollable.
            .iter()
            .take(rect.height as usize)
            .map(|member| {
                let rendered = to_lua_user(lua, member)
                    .ok()
                    .and_then(|tbl| self.render_user(lua, &tbl).ok())
                    .unwrap_or_default();

                if rendered.is_empty() {
                    ListItem::new(member.user.name().to_string())
                } else {
                    ListItem::new(Line::from(rendered))
                }
            });

        // The theme owns the userlist title; default to the plain buffer name
        // when no `userlist_title` formatter is set (or it yields nothing).
        let mut spans = self
            .format_spans(lua, "userlist_title", title.to_string())
            .unwrap_or_default();
        if spans.is_empty() {
            spans.push(Span::raw(title.to_string()));
        }

        let list = List::new(users).block(Block::default().title(spans).borders(Borders::LEFT));
        f.render_widget(list, rect);
    }

    /// Resolves the focused buffer along with its backend metadata.
    fn focused<'a>(
        &self,
        state: &'a State,
        view: &'a ViewState,
    ) -> Option<(&'a BufferId, &'a ChatBuffer, &'a BackendInfo, &'a str)> {
        let buffer_id = view.focused.as_ref()?;
        let buffer = state.buffers.get(buffer_id)?;
        let backend_state = state.backends.get(&buffer_id.backend)?;
        Some((
            buffer_id,
            buffer,
            &backend_state.info,
            backend_state.nickname.as_str(),
        ))
    }

    pub fn render(
        &mut self,
        f: &mut ratatui::Frame,
        state: &State,
        view: &mut ViewState,
        lua: &mlua::Lua,
        input: &Input,
    ) {
        // Populate the render context (multi_backend, focused_buffer, ...) before
        // building the bar, as the theme's render_buffer_bar reads those globals.
        let _ = self.update_render_context(lua, view, state);

        // Build the bar first so the layout can size its region to fit the rows
        // the theme returned, capped so the message area never collapses.
        let (bar_lines, bar_style, bar_tab_widths, bar_scroll_mode) =
            self.build_buffer_bar(state, lua);
        let max_bar_height = f.area().height.saturating_sub(3);
        let bar_height = (bar_lines.len() as u16).clamp(1, max_bar_height.max(1));

        let layout = self.get_layout(bar_height);
        let chunks = layout.split(f.area());

        let members = self
            .focused(state, view)
            .map(|(id, buffer, _, _)| (buffer.label(&id.target).to_string(), &buffer.members));

        let mut userlist_rect = None;
        let mut split_x = None;

        let msg_rect = match members {
            Some((title, members)) if members.len() > 1 => {
                // The sidebar takes a fixed column width (user-resizable) and the
                // message area gets the rest; `sidebar_constraint_width` clamps so
                // the message area can never collapse.
                let sidebar = view.sidebar_constraint_width(chunks[0].width);
                let split = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Min(0), Constraint::Length(sidebar)])
                    .split(chunks[0]);

                self.render_users(f, members, lua, &title, split[1]);
                userlist_rect = Some(split[1]);
                split_x = Some(split[1].x);
                split[0]
            }
            _ => chunks[0],
        };

        view.viewport_height = msg_rect.height;

        let (reaction_hits, image_draws, message_rows) =
            self.render_messages(f, state, view, lua, msg_rect);
        self.draw_images(f, image_draws);

        // Compute horizontal scroll so the focused tab stays visible.
        let focused_index = view
            .focused
            .as_ref()
            .and_then(|id| state.buffers.keys().position(|k| k == id));
        view.bar_x_scroll = buffer_bar_scroll(
            &bar_tab_widths,
            focused_index,
            chunks[2].width,
            view.bar_x_scroll,
            bar_scroll_mode,
        );

        f.render_widget(
            Paragraph::new(Text::from(bar_lines))
                .style(bar_style)
                .scroll((0, view.bar_x_scroll)),
            chunks[2],
        );
        let can_post = view
            .focused
            .as_ref()
            .and_then(|id| state.buffers.get(id))
            .map(|buffer| buffer.can_post)
            .unwrap_or(true);
        self.render_input(f, view, input, can_post, chunks[1]);

        // Record this frame's hit regions so the input handler can resolve mouse
        // clicks without re-deriving the layout. Built last, after the bar's Lua
        // context is in place, so the tab widths match what was drawn.
        let bar_tabs =
            self.bar_tabs_from_widths(state, chunks[2], &bar_tab_widths, view.bar_x_scroll);
        view.layout = LayoutMap {
            message_rect: msg_rect,
            bar_rect: chunks[2],
            bar_tabs,
            userlist_rect,
            userlist_first_member: 0,
            split_x,
            reactions: reaction_hits,
            message_rows,
        };

        // Highlight the app-level selection by reversing the covered cells of the
        // message area, drawn after the messages but before the menu so the menu
        // stays on top. Line-granular for v1: the whole message-area width of
        // every selected row is reversed.
        self.render_selection_highlight(f, view);

        // Highlight the message selected in message-select mode by reversing its
        // first row, mirroring the text-selection highlight so it stays
        // theme-agnostic. The quick-reaction bar drawn below it reinforces it.
        self.render_selected_message_highlight(f, view);

        // The debug log pane floats over the frame (under the context menu, which
        // is drawn last so it stays on top).
        if view.debug_open {
            self.render_debug_pane(f);
        }

        // The context menu is drawn last so it floats over everything. The full
        // synchronized repaint each frame (see `ui.rs`) means a `Clear` plus the
        // bordered list is all that is needed - there is no incremental diff to
        // fight. The resolved rect is stored back on the menu so the input handler
        // hit-tests clicks against the exact geometry that was drawn.
        if view.menu.open {
            self.render_context_menu(f, view);
        }
    }

    /// Draws the `:debug` log overlay: a centered bordered pane showing the most
    /// recent captured log lines (oldest at top, newest at bottom), colored by
    /// level. Reads the shared in-memory buffer directly, so no view state beyond
    /// the open flag is needed.
    fn render_debug_pane(&self, f: &mut ratatui::Frame) {
        let area = f.area();
        // Centered, leaving a 2-cell margin on each side.
        let width = area.width.saturating_sub(4);
        let height = area.height.saturating_sub(4);
        if width == 0 || height == 0 {
            return;
        }
        let rect = Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height - height) / 2,
            width,
            height,
        };

        let block = Block::default()
            .title("Debug log (:debug to close)")
            .borders(Borders::ALL);
        let inner = block.inner(rect);

        // Request exactly as many lines as fit, so the visible window always shows
        // the most recent output.
        let lines = crate::logging::recent(inner.height as usize);
        let items: Vec<ListItem> = if lines.is_empty() {
            vec![ListItem::new(Line::from(Span::styled(
                "(no log output yet)",
                Style::default().fg(Color::DarkGray),
            )))]
        } else {
            lines
                .iter()
                .map(|line| ListItem::new(debug_log_line(line)))
                .collect()
        };

        f.render_widget(Clear, rect);
        f.render_widget(List::new(items).block(block), rect);
    }

    /// Reverses the cells of every row the selection covers, clamped to the
    /// message area so the highlight never bleeds into the bar, input line, or
    /// user list. A no-op when there is no selection. Cell coordinates are
    /// validated against the frame buffer (`cell_mut` returns `Option`) so a
    /// selection captured from a larger earlier frame cannot panic after a
    /// resize.
    fn render_selection_highlight(&self, f: &mut ratatui::Frame, view: &ViewState) {
        let Some(selection) = view.selection else {
            return;
        };

        let rect = view.layout.message_rect;
        let reversed = Style::default().add_modifier(Modifier::REVERSED);
        let rows = selection.selected_rows();
        let top = rect.y;
        let bottom = rect.y.saturating_add(rect.height);
        let buf = f.buffer_mut();

        for y in rows {
            if y < top || y >= bottom {
                continue;
            }
            for x in rect.x..rect.right() {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_style(reversed);
                }
            }
        }
    }

    /// Reverses the first row of the message selected in message-select mode,
    /// clamped to the message area. A no-op when nothing is selected or the
    /// selected message is scrolled off screen (absent from the row hit map).
    fn render_selected_message_highlight(&self, f: &mut ratatui::Frame, view: &ViewState) {
        let Some(index) = view.selected_message else {
            return;
        };
        let Some((rect, _)) = view.layout.message_rows.iter().find(|(_, i)| *i == index) else {
            return;
        };

        let area = view.layout.message_rect;
        let y = rect.y;
        if y < area.y || y >= area.y.saturating_add(area.height) {
            return;
        }
        let reversed = Style::default().add_modifier(Modifier::REVERSED);
        let buf = f.buffer_mut();
        for x in area.x..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_style(reversed);
            }
        }
    }

    /// Draws the floating context menu and records its on-screen rectangle on
    /// `view.menu` for the input handler to read back. Plain styling: the
    /// highlighted row is reversed; no theme formatter is involved in v1.
    fn render_context_menu(&self, f: &mut ratatui::Frame, view: &mut ViewState) {
        let rect = view.menu.resolved_rect(f.area());
        view.menu.rect = rect;

        let items: Vec<ListItem> = view
            .menu
            .items
            .iter()
            .map(|item| ListItem::new(item.label.clone()))
            .collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

        let mut list_state = ListState::default();
        list_state.select(Some(view.menu.selected));

        f.render_widget(Clear, rect);
        f.render_stateful_widget(list, rect, &mut list_state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use ratatui::style::Color;
    use ratatui::text::Span;

    use crate::tui::lua::create_tirc_theme_lua_module;

    fn run_lua_code(lua: &mlua::Lua, code: &str) -> mlua::Result<mlua::Value> {
        lua.load(code).eval()
    }

    fn render_lua_table_to_spans<'lua>(
        lua: &'lua mlua::Lua,
        renderer: &'lua Renderer,
        table: &'lua str,
    ) -> Result<Vec<Span<'lua>>, anyhow::Error> {
        let value = run_lua_code(lua, table)?;
        renderer.lua_value_to_spans(lua, value)
    }

    #[test]
    fn test_lua_value_to_spans() -> anyhow::Result<(), anyhow::Error> {
        let renderer = Renderer::new();
        let lua = mlua::Lua::new();
        let spans = render_lua_table_to_spans(
            &lua,
            &renderer,
            indoc! {"
                { 'Hello', ', ', 'World!' }
            "},
        )?;
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "Hello");
        assert_eq!(spans[1].content, ", ");
        assert_eq!(spans[2].content, "World!");
        Ok(())
    }

    #[test]
    fn test_lua_value_to_spans_nested() -> anyhow::Result<(), anyhow::Error> {
        let renderer = Renderer::new();
        let lua = mlua::Lua::new();
        let spans = render_lua_table_to_spans(
            &lua,
            &renderer,
            indoc! {"
                { 'Hello', { ', ' }, 'World!' }
            "},
        )?;
        assert_eq!(spans.len(), 3);
        Ok(())
    }

    #[test]
    fn test_lua_value_to_styled_spans() -> anyhow::Result<(), anyhow::Error> {
        let renderer = Renderer::new();
        let lua = mlua::Lua::new();
        create_tirc_theme_lua_module(&lua)?;
        let spans = render_lua_table_to_spans(
            &lua,
            &renderer,
            indoc! {"
                local theme = require('tirc.tui.theme')
                local blue = theme.style { fg = 'blue' }

                return { 'a', blue }
            "},
        )?;
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "a");
        assert_eq!(spans[0].style.fg, Some(Color::Blue));
        Ok(())
    }

    #[test]
    fn test_two_child_tables_are_not_a_style() -> anyhow::Result<(), anyhow::Error> {
        // `{ {..}, {..} }` is two child span-lists, not a styled span: without
        // the style marker the renderer must treat it as a list.
        let renderer = Renderer::new();
        let lua = mlua::Lua::new();
        create_tirc_theme_lua_module(&lua)?;
        let spans = render_lua_table_to_spans(
            &lua,
            &renderer,
            indoc! {"
                { { 'a' }, { 'b' } }
            "},
        )?;
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "a");
        assert_eq!(spans[1].content, "b");
        Ok(())
    }

    #[test]
    fn test_lua_value_to_styled_spans_deeply_nested() -> anyhow::Result<(), anyhow::Error> {
        let renderer = Renderer::new();
        let lua = mlua::Lua::new();
        create_tirc_theme_lua_module(&lua)?;
        let spans = render_lua_table_to_spans(
            &lua,
            &renderer,
            indoc! {"
                local theme = require('tirc.tui.theme')

                local blue = theme.style { fg = 'blue' }
                local green = theme.style { fg = 'green' }
                local darkgray = theme.style { fg = 'darkgray', bg = 'white' }

                return { { 'a', blue }, { { 'b', { 'c', { 'd', green }, 'e' } }, darkgray }, 'f' }
            "},
        )?;
        assert_eq!(spans.len(), 6);
        assert_eq!(spans[0].content, "a");
        assert_eq!(spans[0].style.fg, Some(Color::Blue));
        assert_eq!(spans[1].content, "b");
        assert_eq!(spans[1].style.fg, Some(Color::DarkGray));
        assert_eq!(spans[1].style.bg, Some(Color::White));
        assert_eq!(spans[3].content, "d");
        assert_eq!(spans[3].style.fg, Some(Color::Green));
        assert_eq!(spans[3].style.bg, Some(Color::White));
        assert_eq!(spans[5].content, "f");
        assert_eq!(spans[5].style.fg, None);
        Ok(())
    }

    #[test]
    fn lua_value_to_rows_yields_one_line_per_row() -> anyhow::Result<(), anyhow::Error> {
        let renderer = Renderer::new();
        let lua = mlua::Lua::new();
        let value = run_lua_code(&lua, "{ rows = { { 'a' }, { 'b', 'c' } } }")?;
        let rows = renderer.rows_to_lines_and_widths(&lua, value)?.0;
        assert_eq!(rows.len(), 2);
        Ok(())
    }

    #[test]
    fn lua_value_to_rows_treats_bare_value_as_single_row() -> anyhow::Result<(), anyhow::Error> {
        let renderer = Renderer::new();
        let lua = mlua::Lua::new();
        let value = run_lua_code(&lua, "{ 'x', 'y' }")?;
        let rows = renderer.rows_to_lines_and_widths(&lua, value)?.0;
        assert_eq!(rows.len(), 1);
        Ok(())
    }

    #[test]
    fn build_bar_tabs_produces_contiguous_hit_boxes() -> anyhow::Result<(), anyhow::Error> {
        use crate::backends::BackendInfo;
        use crate::core::{
            BackendId, ChatEvent, MessageBody, MsgKind, Protocol, TargetId, UserRef,
        };
        use crate::ui::{State, ViewState};

        let lua = mlua::Lua::new();
        crate::config::register_builtin_modules(&lua)?;
        lua.load("require('tirc.tui.themes.default'):setup({})")
            .exec()?;

        let backend = BackendId(0);
        let mut state = State::new();
        state.register_backend(BackendInfo {
            id: backend,
            protocol: Protocol::Irc,
            name: "irc.example.com".to_string(),
        });
        // Create two channel buffers in addition to the status buffer.
        for channel in ["#a", "#bb"] {
            state.apply(
                backend,
                ChatEvent::Message {
                    target: TargetId::from(channel),
                    id: None,
                    sender: UserRef::new("alice"),
                    body: MessageBody::plain("hi"),
                    kind: MsgKind::Text,
                    echo_of: None,
                    time: None,
                },
            );
        }

        let mut view = ViewState::new();
        view.focus(BufferId::status(backend));

        let renderer = Renderer::new();
        // Populate the _tirc globals the tab formatter reads before measuring.
        renderer.update_render_context(&lua, &view, &state)?;

        let bar_rect = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 1,
        };
        let (_, _, widths, _) = renderer.build_buffer_bar(&state, &lua);
        let tabs = renderer.bar_tabs_from_widths(&state, bar_rect, &widths, 0);

        assert_eq!(tabs.len(), state.buffers.len(), "one hit box per buffer");
        assert_eq!(tabs[0].0.x, bar_rect.x, "first tab starts at the bar's x");
        for pair in tabs.windows(2) {
            let (prev, _) = &pair[0];
            let (next, _) = &pair[1];
            assert_eq!(
                next.x,
                prev.x + prev.width,
                "tabs are contiguous with no gaps or overlaps"
            );
            assert!(prev.width > 0, "each tab has a measurable width");
        }
        Ok(())
    }

    /// The slanted theme inserts separator spans around each tab. Its hit boxes
    /// must still total the full rendered bar width: measuring the actual row
    /// elements (not a separate per-tab re-measure) is what makes this hold.
    #[test]
    fn slanted_theme_hit_boxes_cover_full_bar_width() -> anyhow::Result<(), anyhow::Error> {
        use crate::backends::BackendInfo;
        use crate::core::BackendId;
        use crate::core::{ChatEvent, MessageBody, MsgKind, Protocol, TargetId, UserRef};
        use crate::ui::{State, ViewState};

        let lua = mlua::Lua::new();
        crate::config::register_builtin_modules(&lua)?;
        lua.load("require('tirc.tui.themes.slanted'):setup({})")
            .exec()?;

        let backend = BackendId(0);
        let mut state = State::new();
        state.register_backend(BackendInfo {
            id: backend,
            protocol: Protocol::Irc,
            name: "irc.example.com".to_string(),
        });
        for channel in ["#a", "#bb"] {
            state.apply(
                backend,
                ChatEvent::Message {
                    target: TargetId::from(channel),
                    id: None,
                    sender: UserRef::new("alice"),
                    body: MessageBody::plain("hi"),
                    kind: MsgKind::Text,
                    echo_of: None,
                    time: None,
                },
            );
        }

        let mut view = ViewState::new();
        view.focus(BufferId::status(backend));

        let renderer = Renderer::new();
        renderer.update_render_context(&lua, &view, &state)?;

        let (lines, _, widths, _) = renderer.build_buffer_bar(&state, &lua);
        let bar_rect = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 1,
        };
        let tabs = renderer.bar_tabs_from_widths(&state, bar_rect, &widths, 0);

        assert_eq!(tabs.len(), state.buffers.len(), "one hit box per buffer");

        // The hit boxes must span exactly the drawn bar: the right edge of the
        // last tab equals the rendered line width, separators included.
        let drawn_width: u16 = lines[0].spans.iter().map(|s| s.width() as u16).sum();
        let last = tabs.last().expect("at least one tab");
        assert_eq!(
            last.0.x + last.0.width,
            bar_rect.x + drawn_width,
            "hit boxes cover the full rendered bar (separators attributed to tabs)"
        );
        for pair in tabs.windows(2) {
            assert_eq!(pair[1].0.x, pair[0].0.x + pair[0].0.width, "contiguous");
        }
        Ok(())
    }

    #[test]
    fn default_theme_render_buffer_bar_returns_single_row() -> anyhow::Result<(), anyhow::Error> {
        let lua = mlua::Lua::new();
        crate::config::register_builtin_modules(&lua)?;
        lua.load("require('tirc.tui.themes.default'):setup({})")
            .exec()?;

        let buffers = lua.create_table()?;
        let tab = lua.create_table()?;
        tab.set("id", "0:#tirc")?;
        tab.set("name", "#tirc")?;
        tab.set("target", "#tirc")?;
        tab.set("backend_id", 0)?;
        tab.set("backend_name", "irc.example.com")?;
        buffers.push(tab)?;

        // Themes read _tirc.buffers in has_unique_name; seed it before calling
        // the formatter so the context matches a real render cycle.
        let tirc_mod: mlua::Table = lua
            .globals()
            .get::<mlua::Table>("package")?
            .get::<mlua::Table>("loaded")?
            .get::<mlua::Table>("_tirc")?;
        tirc_mod.set("buffers", buffers.clone())?;

        let renderer = Renderer::new();
        let value = crate::config::call_formatter(&lua, "render_buffer_bar", &buffers)
            .expect("render_buffer_bar registered")
            .expect("render_buffer_bar callback");
        let rows = renderer.rows_to_lines_and_widths(&lua, value)?.0;

        assert_eq!(rows.len(), 1);
        let text: String = rows[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("#tirc"), "row text was {text:?}");
        Ok(())
    }

    /// The reaction hit boxes must mirror the bottom-to-top `List` placement:
    /// each message's reaction row lands on that message's last drawn line, and
    /// `cum` accumulation across items keeps the older message's row exactly two
    /// rows above the newer one (each message here is body + reaction = 2 rows).
    #[test]
    fn reaction_hit_boxes_track_bottom_to_top_layout() -> anyhow::Result<(), anyhow::Error> {
        use crate::backends::BackendInfo;
        use crate::core::{
            BackendId, ChatEvent, EventId, MessageBody, MsgKind, Protocol, TargetId, UserRef,
        };
        use crate::ui::{State, ViewState};
        use ratatui::{backend::TestBackend, Terminal};

        let lua = mlua::Lua::new();
        crate::config::register_builtin_modules(&lua)?;
        lua.load("require('tirc.tui.themes.default'):setup({})")
            .exec()?;

        let backend = BackendId(0);
        let mut state = State::new();
        state.register_backend(BackendInfo {
            id: backend,
            protocol: Protocol::Irc,
            name: "irc.example.com".to_string(),
        });
        state.set_nickname(backend, "me".to_string());

        // Two confirmed messages, oldest first, each with one reaction.
        for event_id in ["$1", "$2"] {
            state.apply(
                backend,
                ChatEvent::Message {
                    target: TargetId::from("#chan"),
                    id: Some(EventId(event_id.to_string())),
                    sender: UserRef::new("alice"),
                    body: MessageBody::plain("hi"),
                    kind: MsgKind::Text,
                    echo_of: None,
                    time: None,
                },
            );
            state.apply(
                backend,
                ChatEvent::Reaction {
                    target: TargetId::from("#chan"),
                    id: EventId(event_id.to_string()),
                    sender: UserRef::new("bob"),
                    key: "👍".to_string(),
                    add: true,
                },
            );
        }

        let mut view = ViewState::new();
        view.focus(BufferId::new(backend, "#chan"));

        let mut renderer = Renderer::new();
        let mut terminal = Terminal::new(TestBackend::new(40, 12))?;
        terminal.draw(|f| renderer.render(f, &state, &mut view, &lua, &Input::default()))?;

        let hits = &view.layout.reactions;
        assert_eq!(hits.len(), 2, "one pill per reacted message");

        let bottom = view.layout.message_rect.bottom();
        // Newest message ($2) is anchored at the bottom; its reaction row is the
        // bottom-most row. The older message ($1) sits above by its own 2-row
        // height, so its reaction row is two rows higher.
        let y_of = |id: &str| {
            hits.iter()
                .find(|(_, hit)| hit.event_id == EventId(id.to_string()))
                .map(|(rect, _)| rect.y)
        };
        assert_eq!(
            y_of("$2"),
            Some(bottom - 1),
            "newest reaction on bottom row"
        );
        assert_eq!(y_of("$1"), Some(bottom - 3), "older reaction two rows up");

        // Every recorded hit round-trips through `reaction_at`.
        for (rect, hit) in hits {
            assert!(rect.width > 0, "pill has a measurable width");
            assert!(
                rect.x >= view.layout.message_rect.x,
                "pill starts within the message area"
            );
            assert_eq!(
                view.layout.reaction_at(rect.x, rect.y),
                Some(hit),
                "reaction_at resolves the pill it recorded"
            );
        }
        Ok(())
    }

    /// A ready link preview renders its title/description on new lines *below*
    /// the message that contains the URL, indented under the message body - never
    /// on the message's own line.
    #[test]
    fn link_preview_renders_below_message() -> anyhow::Result<(), anyhow::Error> {
        use crate::backends::BackendInfo;
        use crate::core::{
            BackendId, ChatEvent, EventId, MessageBody, MsgKind, Protocol, TargetId, UserRef,
        };
        use crate::ui::{State, ViewState};
        use ratatui::{backend::TestBackend, Terminal};

        let lua = mlua::Lua::new();
        crate::config::register_builtin_modules(&lua)?;
        lua.load("require('tirc.tui.themes.default'):setup({})")
            .exec()?;

        let backend = BackendId(0);
        let mut state = State::new();
        state.register_backend(BackendInfo {
            id: backend,
            protocol: Protocol::Irc,
            name: "irc.example.com".to_string(),
        });
        state.set_nickname(backend, "me".to_string());

        let url = "https://example.com/x";
        state.apply(
            backend,
            ChatEvent::Message {
                target: TargetId::from("#chan"),
                id: Some(EventId("$1".to_string())),
                sender: UserRef::new("alice"),
                body: MessageBody::plain(format!("look {url}")),
                kind: MsgKind::Text,
                echo_of: None,
                time: None,
            },
        );

        let mut view = ViewState::new();
        view.focus(BufferId::new(backend, "#chan"));

        let mut renderer = Renderer::new();
        // Simulate a completed fetch: seed the cache the way the worker would.
        renderer.preview_cache.borrow_mut().insert(
            url.to_string(),
            LinkPreview {
                title: Some("Example Title".to_string()),
                description: Some("Example description".to_string()),
                site_name: Some("Example".to_string()),
                image_path: None,
            },
        );

        let mut terminal = Terminal::new(TestBackend::new(60, 12))?;
        terminal.draw(|f| renderer.render(f, &state, &mut view, &lua, &Input::default()))?;

        let buffer = terminal.backend().buffer().clone();
        let area = buffer.area;
        let row_text = |y: u16| -> String {
            (0..area.width)
                .map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(""))
                .collect()
        };

        let mut msg_row = None;
        let mut title_row = None;
        let mut desc_row = None;
        for y in area.top()..area.bottom() {
            let line = row_text(y);
            if line.contains("example.com") {
                msg_row = Some(y);
            }
            if line.contains("Example Title") {
                title_row = Some(y);
            }
            if line.contains("Example description") {
                desc_row = Some(y);
            }
        }

        let msg_row = msg_row.expect("message with the URL is drawn");
        let title_row = title_row.expect("preview title is drawn");
        let desc_row = desc_row.expect("preview description is drawn");

        // The preview rows sit strictly below the message (greater y), in order.
        assert!(title_row > msg_row, "title below message");
        assert!(desc_row > title_row, "description below title");

        // The message line does not itself carry the preview text.
        assert!(!row_text(msg_row).contains("Example Title"));

        // Preview rows are indented under the message body (not at column 0).
        assert!(
            row_text(title_row).starts_with(' '),
            "preview title is indented"
        );

        Ok(())
    }

    /// When the viewport is too short to fit the message together with its
    /// preview, the preview is dropped but the message is still rendered (rather
    /// than `List` dropping the whole over-tall item and hiding the message).
    #[test]
    fn link_preview_dropped_when_it_would_not_fit() -> anyhow::Result<(), anyhow::Error> {
        use crate::backends::BackendInfo;
        use crate::core::{
            BackendId, ChatEvent, EventId, MessageBody, MsgKind, Protocol, TargetId, UserRef,
        };
        use crate::ui::{State, ViewState};
        use ratatui::{backend::TestBackend, Terminal};

        let lua = mlua::Lua::new();
        crate::config::register_builtin_modules(&lua)?;
        lua.load("require('tirc.tui.themes.default'):setup({})")
            .exec()?;

        let backend = BackendId(0);
        let mut state = State::new();
        state.register_backend(BackendInfo {
            id: backend,
            protocol: Protocol::Irc,
            name: "irc.example.com".to_string(),
        });
        state.set_nickname(backend, "me".to_string());

        let url = "https://example.com/x";
        state.apply(
            backend,
            ChatEvent::Message {
                target: TargetId::from("#chan"),
                id: Some(EventId("$1".to_string())),
                sender: UserRef::new("alice"),
                body: MessageBody::plain(format!("look {url}")),
                kind: MsgKind::Text,
                echo_of: None,
                time: None,
            },
        );

        let mut view = ViewState::new();
        view.focus(BufferId::new(backend, "#chan"));

        let mut renderer = Renderer::new();
        renderer.preview_cache.borrow_mut().insert(
            url.to_string(),
            LinkPreview {
                title: Some("Example Title".to_string()),
                description: Some("Example description".to_string()),
                site_name: Some("Example".to_string()),
                image_path: None,
            },
        );

        // Very short viewport: room for the message line but not the two preview
        // rows below it.
        let mut terminal = Terminal::new(TestBackend::new(60, 6))?;
        terminal.draw(|f| renderer.render(f, &state, &mut view, &lua, &Input::default()))?;

        let buffer = terminal.backend().buffer().clone();
        let area = buffer.area;
        let mut all = String::new();
        for y in area.top()..area.bottom() {
            for x in 0..area.width {
                all.push_str(buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(""));
            }
        }

        assert!(all.contains("example.com"), "message is still rendered");
        assert!(
            !all.contains("Example Title") && !all.contains("Example description"),
            "preview is dropped when it does not fit"
        );

        Ok(())
    }

    // --- buffer_bar_scroll unit tests ---

    #[test]
    fn bar_scroll_no_overflow_returns_zero() {
        // All tabs fit: no scrolling needed regardless of focused index.
        let widths = [10u16, 10, 10];
        assert_eq!(
            buffer_bar_scroll(&widths, Some(2), 40, 5, BarScrollMode::Follow),
            0
        );
        assert_eq!(
            buffer_bar_scroll(&widths, Some(2), 40, 5, BarScrollMode::Center),
            0
        );
    }

    #[test]
    fn bar_scroll_follow_scrolls_right_to_reveal_tab() {
        // Tabs: [0..10), [10..20), [20..30). Bar width = 15.
        // With prev_scroll=0, focused=2 (tab at [20..30)): tab_end=30 > 0+15=15
        // → scroll = 30 - 15 = 15.
        let widths = [10u16, 10, 10];
        assert_eq!(
            buffer_bar_scroll(&widths, Some(2), 15, 0, BarScrollMode::Follow),
            15
        );
    }

    #[test]
    fn bar_scroll_follow_scrolls_left_to_reveal_tab() {
        // Same tabs, prev_scroll=15, focused=0 (tab at [0..10)): tab_start=0 < 15
        // → scroll = 0.
        let widths = [10u16, 10, 10];
        assert_eq!(
            buffer_bar_scroll(&widths, Some(0), 15, 15, BarScrollMode::Follow),
            0
        );
    }

    #[test]
    fn bar_scroll_follow_does_not_scroll_when_tab_already_visible() {
        // Tabs: [0..10), [10..20), [20..30). Bar width = 15, prev_scroll = 10.
        // Focused=1 is at [10..20). Visible window: [10..25). Tab fully visible.
        let widths = [10u16, 10, 10];
        assert_eq!(
            buffer_bar_scroll(&widths, Some(1), 15, 10, BarScrollMode::Follow),
            10
        );
    }

    #[test]
    fn bar_scroll_center_centers_focused_tab() {
        // Tabs: [0..10), [10..20), [20..30). Bar width = 15. Focused=2.
        // tab_start=20, tab_width=10. offset = 20 - (15-10)/2 = 20 - 2 = 18.
        // max_scroll = 30 - 15 = 15. clamped to 15.
        let widths = [10u16, 10, 10];
        assert_eq!(
            buffer_bar_scroll(&widths, Some(2), 15, 0, BarScrollMode::Center),
            15
        );
        // Focused=1: tab_start=10. offset = 10 - 2 = 8. max_scroll=15 → 8.
        assert_eq!(
            buffer_bar_scroll(&widths, Some(1), 15, 0, BarScrollMode::Center),
            8
        );
    }

    #[test]
    fn bar_scroll_clamps_stale_scroll_after_buffer_closes() {
        // Only one buffer left, width=5, bar=10: no overflow → 0.
        let widths = [5u16];
        assert_eq!(
            buffer_bar_scroll(&widths, Some(0), 10, 20, BarScrollMode::Follow),
            0
        );
    }

    #[test]
    fn bar_scroll_multi_row_guard_returns_zero() {
        // Focused index out of range (multi-row theme).
        let widths = [10u16, 10];
        assert_eq!(
            buffer_bar_scroll(&widths, Some(5), 15, 0, BarScrollMode::Follow),
            0
        );
    }

    #[test]
    fn bar_tabs_hit_boxes_correct_under_scroll() -> anyhow::Result<(), anyhow::Error> {
        // Tabs widths: 10, 10, 10. Bar x=0, width=15, scroll=10.
        // Tab 0 [0..10): fully left of window → excluded.
        // Tab 1 [10..20): rel_start=0, rel_end=min(10,15)=10 → box (x=0, w=10).
        // Tab 2 [20..30): rel_start=10, rel_end=min(20,15)=15 → box (x=10, w=5).
        use crate::backends::BackendInfo;
        use crate::core::{
            BackendId, ChatEvent, MessageBody, MsgKind, Protocol, TargetId, UserRef,
        };
        use crate::ui::{State, ViewState};

        let lua = mlua::Lua::new();
        crate::config::register_builtin_modules(&lua)?;
        lua.load("require('tirc.tui.themes.default'):setup({})")
            .exec()?;

        let backend = BackendId(0);
        let mut state = State::new();
        state.register_backend(BackendInfo {
            id: backend,
            protocol: Protocol::Irc,
            name: "irc.example.com".to_string(),
        });
        // Push messages to create buffers.
        for channel in ["#a", "#bb"] {
            state.apply(
                backend,
                ChatEvent::Message {
                    target: TargetId::from(channel),
                    id: None,
                    sender: UserRef::new("alice"),
                    body: MessageBody::plain("hi"),
                    kind: MsgKind::Text,
                    echo_of: None,
                    time: None,
                },
            );
        }

        let mut view = ViewState::new();
        view.focus(BufferId::status(backend));

        let renderer = Renderer::new();
        renderer.update_render_context(&lua, &view, &state)?;

        let (_, _, widths, _) = renderer.build_buffer_bar(&state, &lua);
        let bar_rect = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 1,
        };
        let total: u16 = widths.iter().sum();

        // Scroll to show the last tab.
        let last_idx = widths.len() - 1;
        let last_tab_start: u16 = widths[..last_idx].iter().sum();
        let last_tab_end = last_tab_start + widths[last_idx];
        let scroll = last_tab_end.saturating_sub(bar_rect.width);

        let tabs = renderer.bar_tabs_from_widths(&state, bar_rect, &widths, scroll);

        // All scrolled-off tabs should still be mapped but may be partially clipped.
        // At minimum the last tab must be fully visible at the right side.
        let last = tabs.last().expect("at least one tab visible");
        let last_screen_end = last.0.x + last.0.width;
        assert!(
            last_screen_end <= bar_rect.x + bar_rect.width,
            "last tab right edge {last_screen_end} must be within bar width {}",
            bar_rect.x + bar_rect.width
        );

        // No hit box must exceed the bar's right boundary.
        for (rect, _) in &tabs {
            assert!(
                rect.x + rect.width <= bar_rect.x + bar_rect.width,
                "tab hit box overflows bar: {rect:?}"
            );
        }

        // With scroll=0 the total hit-box width still equals total rendered width
        // (existing invariant should still hold after the refactor).
        let tabs_no_scroll = renderer.bar_tabs_from_widths(&state, bar_rect, &widths, 0);
        let hit_total: u16 = tabs_no_scroll.iter().map(|(r, _)| r.width).sum();
        assert_eq!(hit_total, total, "zero-scroll: hit boxes cover all tabs");

        Ok(())
    }

    #[test]
    fn date_separator_contains_date() -> anyhow::Result<(), anyhow::Error> {
        let lua = mlua::Lua::new();
        crate::config::register_builtin_modules(&lua)?;
        lua.load("require('tirc.tui.themes.default'):setup({})")
            .exec()?;

        use chrono::TimeZone;
        // June 30, 2026 at noon local time.
        let date = chrono::Local
            .with_ymd_and_hms(2026, 6, 30, 12, 0, 0)
            .unwrap();

        let renderer = Renderer::new();
        let line = renderer.render_date_separator(&lua, &date, 80);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("30 Jun 2026"),
            "expected '30 Jun 2026' in separator, got: {text:?}"
        );
        Ok(())
    }

    /// The oldest day's block gets a date separator at the very top of the list,
    /// not just between day changes, so restored/scrolled-in history is always
    /// dated. Two messages on consecutive days must yield separators for both.
    #[test]
    fn oldest_message_has_date_separator_at_top() -> anyhow::Result<(), anyhow::Error> {
        use crate::backends::BackendInfo;
        use crate::core::{
            BackendId, ChatEvent, MessageBody, MsgKind, Protocol, TargetId, UserRef,
        };
        use crate::ui::{State, ViewState};
        use chrono::TimeZone;
        use ratatui::{backend::TestBackend, Terminal};

        let lua = mlua::Lua::new();
        crate::config::register_builtin_modules(&lua)?;
        lua.load("require('tirc.tui.themes.default'):setup({})")
            .exec()?;

        let backend = BackendId(0);
        let mut state = State::new();
        state.register_backend(BackendInfo {
            id: backend,
            protocol: Protocol::Irc,
            name: "irc.example.com".to_string(),
        });

        // Noon UTC on two consecutive days: local dates stay one day apart under
        // any timezone offset, so the two messages always land on different days.
        let older = chrono::Utc.with_ymd_and_hms(2026, 6, 29, 12, 0, 0).unwrap();
        let newer = chrono::Utc.with_ymd_and_hms(2026, 6, 30, 12, 0, 0).unwrap();
        for (time, text) in [(older, "first"), (newer, "second")] {
            state.apply(
                backend,
                ChatEvent::Message {
                    target: TargetId::from("#chan"),
                    id: None,
                    sender: UserRef::new("alice"),
                    body: MessageBody::plain(text),
                    kind: MsgKind::Text,
                    echo_of: None,
                    time: Some(time),
                },
            );
        }

        let mut view = ViewState::new();
        view.focus(BufferId::new(backend, "#chan"));

        let mut renderer = Renderer::new();
        let mut terminal = Terminal::new(TestBackend::new(60, 12))?;
        terminal.draw(|f| renderer.render(f, &state, &mut view, &lua, &Input::default()))?;

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        // The theme formats separators as `%-d %b %Y`; assert both days appear -
        // the newer one as a between-days separator and the older one at the top.
        let older_label = older
            .with_timezone(&chrono::Local)
            .format("%-d %b %Y")
            .to_string();
        let newer_label = newer
            .with_timezone(&chrono::Local)
            .format("%-d %b %Y")
            .to_string();
        assert!(
            rendered.contains(&older_label),
            "expected oldest date {older_label:?} as a top separator, got: {rendered:?}"
        );
        assert!(
            rendered.contains(&newer_label),
            "expected newer date {newer_label:?} as a between-days separator, got: {rendered:?}"
        );
        Ok(())
    }

    /// A long link-preview description wraps within the message area (across
    /// several rows) instead of overflowing off the right edge on a single row.
    #[test]
    fn link_preview_text_wraps() -> anyhow::Result<(), anyhow::Error> {
        use crate::backends::BackendInfo;
        use crate::core::{
            BackendId, ChatEvent, MessageBody, MsgKind, Protocol, TargetId, UserRef,
        };
        use crate::tui::preview::{LinkPreview, PreviewResult};
        use crate::ui::{State, ViewState};
        use ratatui::{backend::TestBackend, Terminal};

        let lua = mlua::Lua::new();
        crate::config::register_builtin_modules(&lua)?;
        lua.load("require('tirc.tui.themes.default'):setup({})")
            .exec()?;

        let backend = BackendId(0);
        let mut state = State::new();
        state.register_backend(BackendInfo {
            id: backend,
            protocol: Protocol::Irc,
            name: "irc.example.com".to_string(),
        });

        let url = "https://example.com/page";
        state.apply(
            backend,
            ChatEvent::Message {
                target: TargetId::from("#chan"),
                id: None,
                sender: UserRef::new("alice"),
                body: MessageBody::plain(format!("look at {url}")),
                kind: MsgKind::Text,
                echo_of: None,
                time: None,
            },
        );

        let mut view = ViewState::new();
        view.focus(BufferId::new(backend, "#chan"));

        let mut renderer = Renderer::new();
        // Seed the preview cache with a description long enough to wrap several
        // times inside a narrow message area. Distinct head/tail tokens let us
        // assert they land on different rows.
        renderer.insert_link_preview(PreviewResult {
            url: url.to_string(),
            preview: Some(LinkPreview {
                title: Some("Example Title".to_string()),
                description: Some(
                    "HEADSTART one two three four five six seven eight nine ten TAILEND"
                        .to_string(),
                ),
                site_name: None,
                image_path: None,
            }),
        });

        let width = 40u16;
        let height = 20u16;
        let mut terminal = Terminal::new(TestBackend::new(width, height))?;
        terminal.draw(|f| renderer.render(f, &state, &mut view, &lua, &Input::default()))?;

        // Reconstruct the screen rows so we can compare where tokens landed.
        let buffer = terminal.backend().buffer();
        let symbols: Vec<String> = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect();
        let rows: Vec<String> = symbols
            .chunks(width as usize)
            .map(|row| row.concat())
            .collect();

        let head_row = rows.iter().position(|r| r.contains("HEADSTART"));
        let tail_row = rows.iter().position(|r| r.contains("TAILEND"));

        let full: String = rows.join("\n");
        assert!(
            head_row.is_some() && tail_row.is_some(),
            "expected both HEADSTART and TAILEND to be visible, got:\n{full}"
        );
        assert_ne!(
            head_row, tail_row,
            "description did not wrap: HEADSTART and TAILEND share a row, got:\n{full}"
        );
        // The gutter decoration (theme's left glyph) is repeated on the wrapped
        // continuation row, not just the first row of the description.
        let (head, tail) = (head_row.unwrap(), tail_row.unwrap());
        assert!(
            rows[head].contains('\u{258e}') && rows[tail].contains('\u{258e}'),
            "expected the gutter glyph on both wrapped description rows, got:\n{full}"
        );
        // The message's timestamp separator continues down the preview rows too,
        // so `▏` is present alongside the `▎` gutter on both wrapped rows.
        assert!(
            rows[head].contains('\u{258f}') && rows[tail].contains('\u{258f}'),
            "expected the timestamp separator on both wrapped description rows, got:\n{full}"
        );
        Ok(())
    }

    /// Without a graphics picker (the test environment has no terminal), an image
    /// attachment renders as its `[image: name]` fallback line rather than
    /// vanishing, so the media is never silently dropped.
    #[test]
    fn image_attachment_without_graphics_renders_fallback_line() -> anyhow::Result<(), anyhow::Error>
    {
        use crate::backends::BackendInfo;
        use crate::core::{
            Attachment, AttachmentKind, BackendId, ChatEvent, EventId, MessageBody, MsgKind,
            Protocol, TargetId, UserRef,
        };
        use crate::ui::{State, ViewState};
        use ratatui::{backend::TestBackend, Terminal};

        let lua = mlua::Lua::new();
        crate::config::register_builtin_modules(&lua)?;
        lua.load("require('tirc.tui.themes.default'):setup({})")
            .exec()?;

        let backend = BackendId(0);
        let mut state = State::new();
        state.register_backend(BackendInfo {
            id: backend,
            protocol: Protocol::Matrix,
            name: "matrix.example.com".to_string(),
        });

        state.apply(
            backend,
            ChatEvent::Message {
                target: TargetId::from("#chan"),
                id: Some(EventId("$1".to_string())),
                sender: UserRef::new("alice"),
                body: MessageBody::with_attachments(
                    "",
                    vec![Attachment {
                        kind: AttachmentKind::Image,
                        name: "cat.png".to_string(),
                        url: Some("/cache/cat.png".to_string()),
                        source: None,
                        mime: Some("image/png".to_string()),
                        local_path: None,
                    }],
                ),
                kind: MsgKind::Text,
                echo_of: None,
                time: None,
            },
        );

        let mut view = ViewState::new();
        view.focus(BufferId::new(backend, "#chan"));

        let mut renderer = Renderer::new();
        let mut terminal = Terminal::new(TestBackend::new(60, 12))?;
        terminal.draw(|f| renderer.render(f, &state, &mut view, &lua, &Input::default()))?;

        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            text.contains("[image: cat.png]"),
            "expected image fallback line, got: {text:?}"
        );
        Ok(())
    }

    /// A read-only room (BufferPostPolicy can_post=false) shows the input hint while
    /// composing, and a postable room does not, so the user gets feedback before a
    /// send would silently fail.
    #[test]
    fn read_only_room_hints_in_insert_mode() -> anyhow::Result<(), anyhow::Error> {
        use crate::backends::BackendInfo;
        use crate::core::{BackendId, ChatEvent, Protocol, TargetId};
        use crate::ui::{Mode, State, ViewState};
        use ratatui::{backend::TestBackend, Terminal};

        let lua = mlua::Lua::new();
        crate::config::register_builtin_modules(&lua)?;
        lua.load("require('tirc.tui.themes.default'):setup({})")
            .exec()?;

        let backend = BackendId(0);
        let mut state = State::new();
        state.register_backend(BackendInfo {
            id: backend,
            protocol: Protocol::Matrix,
            name: "matrix.example.com".to_string(),
        });
        state.apply(
            backend,
            ChatEvent::BufferPostPolicy {
                target: TargetId::from("#chan"),
                can_post: false,
            },
        );

        fn render_to_text(
            state: &State,
            lua: &mlua::Lua,
            backend: BackendId,
            mode: Mode,
        ) -> anyhow::Result<String, anyhow::Error> {
            let mut view = ViewState::new();
            view.focus(BufferId::new(backend, "#chan"));
            view.mode = mode;
            let mut renderer = Renderer::new();
            let mut terminal = Terminal::new(TestBackend::new(60, 12))?;
            terminal.draw(|f| renderer.render(f, state, &mut view, lua, &Input::default()))?;
            Ok(terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect())
        }

        assert!(
            render_to_text(&state, &lua, backend, Mode::Insert)?.contains("no permission to post"),
            "expected the read-only hint while composing in a non-postable room"
        );
        // The hint is scoped to composing: Normal mode does not show it.
        assert!(
            !render_to_text(&state, &lua, backend, Mode::Normal)?.contains("no permission to post"),
            "did not expect the read-only hint outside Insert mode"
        );

        // A postable buffer never shows the hint, even in Insert mode.
        state.apply(
            backend,
            ChatEvent::BufferPostPolicy {
                target: TargetId::from("#chan"),
                can_post: true,
            },
        );
        assert!(
            !render_to_text(&state, &lua, backend, Mode::Insert)?.contains("no permission to post"),
            "did not expect the hint in a postable room"
        );
        Ok(())
    }

    /// The first render of an image whose bytes are downloaded but not yet decoded
    /// shows the textual fallback and enqueues one background `DecodeRequest`
    /// (never blocking the frame). Once the decoded protocol is handed back via
    /// `insert_decoded`, a second render draws it inline and the fallback is gone.
    #[test]
    fn image_decode_is_requested_then_rendered_inline() -> anyhow::Result<(), anyhow::Error> {
        use crate::backends::BackendInfo;
        use crate::core::{
            Attachment, AttachmentKind, BackendId, ChatEvent, EventId, MessageBody, MsgKind,
            Protocol, TargetId, UserRef,
        };
        use crate::ui::{State, ViewState};
        use ratatui::{backend::TestBackend, Terminal};
        use ratatui_image::{picker::Picker, Resize};

        let lua = mlua::Lua::new();
        crate::config::register_builtin_modules(&lua)?;
        lua.load("require('tirc.tui.themes.default'):setup({})")
            .exec()?;

        let backend = BackendId(0);
        let mut state = State::new();
        state.register_backend(BackendInfo {
            id: backend,
            protocol: Protocol::Matrix,
            name: "matrix.example.com".to_string(),
        });

        let path = PathBuf::from("/cache/cat.png");
        state.apply(
            backend,
            ChatEvent::Message {
                target: TargetId::from("#chan"),
                id: Some(EventId("$1".to_string())),
                sender: UserRef::new("alice"),
                body: MessageBody::with_attachments(
                    "",
                    vec![Attachment {
                        kind: AttachmentKind::Image,
                        name: "cat.png".to_string(),
                        url: Some("/cache/cat.png".to_string()),
                        source: None,
                        mime: Some("image/png".to_string()),
                        local_path: Some(path.clone()),
                    }],
                ),
                kind: MsgKind::Text,
                echo_of: None,
                time: None,
            },
        );

        let mut view = ViewState::new();
        view.focus(BufferId::new(backend, "#chan"));

        // Simulate a terminal with graphics support, wiring the decode channel the
        // main loop would own.
        let mut renderer = Renderer::new();
        renderer.enable_images();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DecodeRequest>();
        renderer.set_decode_sender(tx);

        let mut terminal = Terminal::new(TestBackend::new(60, 12))?;
        terminal.draw(|f| renderer.render(f, &state, &mut view, &lua, &Input::default()))?;

        let rendered = |terminal: &Terminal<TestBackend>| -> String {
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect()
        };

        // First frame: fallback shown, exactly one decode requested for our path.
        assert!(
            rendered(&terminal).contains("[image: cat.png]"),
            "expected fallback while decode is pending"
        );
        let request = rx.try_recv().expect("a decode request was enqueued");
        assert_eq!(request.path, path);
        assert!(
            rx.try_recv().is_err(),
            "only one request should be enqueued"
        );

        // Hand back a decoded protocol fitted to the same area the renderer asked
        // for (halfblocks needs no terminal query, so it works headless).
        let picker = Picker::halfblocks();
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::new(4, 4));
        let protocol = picker
            .new_protocol(image, request.avail, Resize::Fit(None))
            .expect("encode stub protocol");
        renderer.insert_decoded(DecodedImage {
            path: path.clone(),
            protocol: Some(EncodedImage::Widget(protocol)),
        });

        // Second frame: drawn inline, no fallback line, and no further request.
        terminal.draw(|f| renderer.render(f, &state, &mut view, &lua, &Input::default()))?;
        assert!(
            !rendered(&terminal).contains("[image: cat.png]"),
            "expected inline image after decode, not the fallback"
        );
        assert!(
            rx.try_recv().is_err(),
            "no re-request once the image is cached"
        );

        Ok(())
    }

    /// The theme's `▎` preview gutter (and the message's `▏` timestamp
    /// separator) continue down the rows reserved for the preview thumbnail, so
    /// the left decoration runs unbroken beside the image instead of stopping
    /// at the last text row.
    #[test]
    fn link_preview_gutter_continues_beside_thumbnail() -> anyhow::Result<(), anyhow::Error> {
        use crate::backends::BackendInfo;
        use crate::core::{
            BackendId, ChatEvent, EventId, MessageBody, MsgKind, Protocol, TargetId, UserRef,
        };
        use crate::ui::{State, ViewState};
        use ratatui::{backend::TestBackend, Terminal};
        use ratatui_image::{picker::Picker, Resize};

        let lua = mlua::Lua::new();
        crate::config::register_builtin_modules(&lua)?;
        lua.load("require('tirc.tui.themes.default'):setup({})")
            .exec()?;

        let backend = BackendId(0);
        let mut state = State::new();
        state.register_backend(BackendInfo {
            id: backend,
            protocol: Protocol::Irc,
            name: "irc.example.com".to_string(),
        });
        state.set_nickname(backend, "me".to_string());

        let url = "https://example.com/x";
        state.apply(
            backend,
            ChatEvent::Message {
                target: TargetId::from("#chan"),
                id: Some(EventId("$1".to_string())),
                sender: UserRef::new("alice"),
                body: MessageBody::plain(format!("look {url}")),
                kind: MsgKind::Text,
                echo_of: None,
                time: None,
            },
        );

        let mut view = ViewState::new();
        view.focus(BufferId::new(backend, "#chan"));

        let thumb = PathBuf::from("/cache/og.png");
        let mut renderer = Renderer::new();
        renderer.enable_images();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DecodeRequest>();
        renderer.set_decode_sender(tx);
        // Simulate a completed fetch (with a downloaded og:image) the way the
        // preview worker would deliver it.
        renderer.preview_cache.borrow_mut().insert(
            url.to_string(),
            LinkPreview {
                title: Some("Example Title".to_string()),
                description: None,
                site_name: None,
                image_path: Some(thumb.clone()),
            },
        );

        // First frame requests the thumbnail decode; answer it headlessly.
        let mut terminal = Terminal::new(TestBackend::new(60, 12))?;
        terminal.draw(|f| renderer.render(f, &state, &mut view, &lua, &Input::default()))?;
        let request = rx.try_recv().expect("a thumbnail decode was enqueued");
        assert_eq!(request.path, thumb);
        let picker = Picker::halfblocks();
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::new(4, 4));
        let protocol = picker
            .new_protocol(image, request.avail, Resize::Fit(None))
            .expect("encode stub protocol");
        let thumb_size = protocol.size();
        assert!(thumb_size.height > 0, "stub thumbnail reserves rows");
        renderer.insert_decoded(DecodedImage {
            path: thumb.clone(),
            protocol: Some(EncodedImage::Widget(protocol)),
        });

        // Second frame draws the thumbnail on its reserved rows.
        terminal.draw(|f| renderer.render(f, &state, &mut view, &lua, &Input::default()))?;

        let buffer = terminal.backend().buffer().clone();
        let area = buffer.area;
        let row_text = |y: u16| -> String {
            (0..area.width)
                .map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(""))
                .collect()
        };

        let title_row = (area.top()..area.bottom())
            .find(|&y| row_text(y).contains("Example Title"))
            .expect("preview title is drawn");
        let gutter_col = row_text(title_row)
            .chars()
            .position(|c| c == '\u{258e}')
            .expect("title row carries the gutter glyph");

        // Every reserved thumbnail row below the title repeats both bars, with
        // the gutter in the same column as on the text row.
        for y in title_row + 1..=title_row + thumb_size.height {
            let line = row_text(y);
            assert!(
                line.contains('\u{258f}'),
                "expected the timestamp separator on thumbnail row {y}, got:\n{line}"
            );
            assert_eq!(
                line.chars().position(|c| c == '\u{258e}'),
                Some(gutter_col),
                "expected the preview gutter on thumbnail row {y}, got:\n{line}"
            );
        }

        Ok(())
    }
}
