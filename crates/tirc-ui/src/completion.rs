//! The completion engine: sources, triggers, span extraction, fuzzy ranking,
//! and the popup state the renderer draws.
//!
//! A [`CompletionSource`] pairs a declarative [`Trigger`] (what part of the
//! input activates it, per [`Mode`]) with a `complete` function producing
//! [`CompletionItem`]s for the extracted query. Command-mode name completion
//! is derived directly from the command registry ([`commands`]); the emoji
//! source is a builtin [`CompletionSource`]; additional sources can be
//! registered from Lua via `tirc.register_completion_source` and are
//! consulted after the builtins. All indices in this module are **char
//! indices**, matching
//! `tui_input::Input::cursor()`; conversion to byte offsets happens only at
//! the string-slicing boundary.

use mlua::Lua;
use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Matcher, Utf32Str};
use ratatui::layout::Rect;

use super::commands::{self, ArgKind, Resolution};
use super::state::{Mode, State};
use tirc_core::BufferId;
use tirc_lua::runtime::completion_sources_registry;

/// Upper bound on the items a single query returns; the popup scrolls within
/// its ~8 visible rows, so anything beyond this is noise.
pub const MAX_ITEMS: usize = 50;

/// A successful query: the char range of the input to replace and the items
/// to offer.
type QueryResult = ((usize, usize), Vec<CompletionItem>);

/// One row of the completion popup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    /// What the popup shows, e.g. `😄 :smile:` or `join`.
    pub label: String,
    /// The text spliced into the input over the trigger span, e.g. `😄 ` or
    /// `join `.
    pub insert: String,
}

/// What part of the input activates a source. Declarative so Lua-registered
/// sources can describe their trigger as plain data and span extraction stays
/// uniform (and in Rust) across all sources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// Complete the first word of the line (command names). Active only while
    /// the cursor is inside that first word.
    LineStart { min_chars: usize },
    /// A sigil char opens a span when it sits at index 0 or after whitespace,
    /// e.g. `:` for emoji shortcodes. `10:30` never triggers.
    Sigil { ch: char, min_chars: usize },
}

impl Trigger {
    /// Extracts the active span for this trigger from `value`/`cursor`, or
    /// `None` when the trigger does not apply. `force` waives the `min_chars`
    /// requirement (Tab force-open).
    fn span(&self, value: &str, cursor: usize, force: bool) -> Option<(usize, usize, String)> {
        match *self {
            Trigger::LineStart { min_chars } => {
                line_start_span(value, cursor, if force { 0 } else { min_chars })
            }
            Trigger::Sigil { ch, min_chars } => {
                sigil_span(value, cursor, ch, if force { 0 } else { min_chars })
            }
        }
    }
}

/// A snapshot of the input line a query runs against.
#[derive(Clone, Copy, Debug)]
pub struct CompletionQuery<'a> {
    pub mode: Mode,
    /// The full input value (`tui_input::Input::value()`).
    pub value: &'a str,
    /// Cursor position as a char index (`tui_input::Input::cursor()`).
    pub cursor: usize,
    /// True when explicitly requested (Tab): triggers match with an empty
    /// query so the full candidate list opens.
    pub force: bool,
    /// Domain state for argument candidates (buffers, members). `None` turns
    /// state-backed argument kinds into no-ops.
    pub state: Option<&'a State>,
    /// The focused buffer, scoping channel/nick candidates.
    pub focused: Option<&'a BufferId>,
}

/// A provider of completion items behind a [`Trigger`].
pub trait CompletionSource {
    fn modes(&self) -> &[Mode];
    fn trigger(&self) -> Trigger;
    fn complete(&self, query: &str, matcher: &mut Matcher) -> Vec<CompletionItem>;
}

/// Runs queries against the builtin sources and any Lua-registered ones.
pub struct CompletionEngine {
    sources: Vec<Box<dyn CompletionSource>>,
    matcher: Matcher,
}

impl Default for CompletionEngine {
    fn default() -> Self {
        CompletionEngine::new()
    }
}

impl CompletionEngine {
    pub fn new() -> Self {
        CompletionEngine {
            sources: vec![Box::new(EmojiSource)],
            matcher: Matcher::default(),
        }
    }

    /// Finds the first source whose mode and trigger match the query and
    /// returns its span (char range to replace) and items. Command-mode
    /// name completion (registry-derived) runs first, then the builtin
    /// sources, then Lua-registered ones; the first span match with a
    /// non-empty item list wins.
    pub fn query(&mut self, ctx: &CompletionQuery, lua: &Lua) -> Option<QueryResult> {
        if ctx.mode == Mode::Command {
            if let Some(result) = self.command_name_query(ctx) {
                return Some(result);
            }
            if let Some(result) = self.command_arg_query(ctx, lua) {
                return Some(result);
            }
        }
        for source in &self.sources {
            if !source.modes().contains(&ctx.mode) {
                continue;
            }
            let Some((start, end, query)) = source.trigger().span(ctx.value, ctx.cursor, ctx.force)
            else {
                continue;
            };
            let items = source.complete(&query, &mut self.matcher);
            if !items.is_empty() {
                return Some(((start, end), items));
            }
        }
        self.query_lua_sources(ctx, lua)
    }

    /// Completes command names at the start of the Command-mode input, from
    /// the command registry: every canonical name and alias, with a trailing
    /// space when the command takes arguments.
    fn command_name_query(&mut self, ctx: &CompletionQuery) -> Option<QueryResult> {
        let min_chars = if ctx.force { 0 } else { 1 };
        let (start, end, query) = line_start_span(ctx.value, ctx.cursor, min_chars)?;
        let items = command_name_items(&query, &mut self.matcher);
        (!items.is_empty()).then_some(((start, end), items))
    }

    /// Completes the argument word under the cursor from the registry spec of
    /// the (prefix-resolved) command in the first word: channels, nicks,
    /// buffer labels, fixed choices, or theme bar styles depending on the
    /// argument position's [`ArgKind`]. On ordinary edits at least one query
    /// char is required so the popup does not open on every space; Tab
    /// force-opens the full list.
    fn command_arg_query(&mut self, ctx: &CompletionQuery, lua: &Lua) -> Option<QueryResult> {
        let (arg_index, start, end, query) = commands::arg_word_span(ctx.value, ctx.cursor)?;
        if !ctx.force && query.is_empty() {
            return None;
        }
        let (name, _) = commands::split_line(ctx.value);
        let spec = match commands::resolve(name, &[]) {
            Resolution::Builtin(spec) => spec,
            _ => return None,
        };
        let kind = *spec.args.get(arg_index)?;
        let names = arg_kind_names(kind, ctx, lua);
        // A trailing space moves on to the next argument; the final declared
        // position completes without one.
        let trailing = arg_index + 1 < spec.args.len();
        let candidates = names.iter().map(|name| {
            let insert = if trailing {
                format!("{name} ")
            } else {
                name.clone()
            };
            (
                name.as_str(),
                CompletionItem {
                    label: name.clone(),
                    insert,
                },
            )
        });
        let items = fuzzy_rank(&mut self.matcher, &query, candidates);
        (!items.is_empty()).then_some(((start, end), items))
    }

    /// Consults sources registered from Lua via
    /// `tirc.register_completion_source`. Trigger matching runs in Rust from
    /// each spec's declarative `trigger` table; on a match the spec's
    /// `complete` function is called with a ctx table and its returned items
    /// are decoded. A misbehaving source is logged and skipped.
    fn query_lua_sources(&mut self, ctx: &CompletionQuery, lua: &Lua) -> Option<QueryResult> {
        let registry = completion_sources_registry(lua).ok()?;
        for spec in registry.sequence_values::<mlua::Table>() {
            let Ok(spec) = spec else { continue };
            match lua_source_query(lua, &spec, ctx) {
                Ok(Some(result)) => return Some(result),
                Ok(None) => {}
                Err(err) => {
                    let name: String = spec.get("name").unwrap_or_default();
                    log::error!(target: "tirc::lua", "completion source '{name}' failed: {err}");
                }
            }
        }
        None
    }
}

/// Runs one Lua source spec against the query: parses its mode and trigger,
/// extracts the span, calls its `complete(ctx)` function, and decodes the
/// returned sequence of `{ label, insert }` tables (a plain string is
/// shorthand for both). `Ok(None)` when the source does not apply.
fn lua_source_query(
    lua: &Lua,
    spec: &mlua::Table,
    ctx: &CompletionQuery,
) -> mlua::Result<Option<QueryResult>> {
    let mode = match spec.get::<String>("mode")?.as_str() {
        "insert" => Mode::Insert,
        "command" => Mode::Command,
        other => {
            return Err(mlua::Error::external(format!(
                "invalid completion mode: {other}"
            )));
        }
    };
    if mode != ctx.mode {
        return Ok(None);
    }

    let trigger = parse_lua_trigger(&spec.get::<mlua::Table>("trigger")?)?;
    let Some((start, end, query)) = trigger.span(ctx.value, ctx.cursor, ctx.force) else {
        return Ok(None);
    };

    let complete: mlua::Function = spec.get("complete")?;
    let call_ctx = lua.create_table()?;
    call_ctx.set("input", ctx.value)?;
    call_ctx.set("cursor", ctx.cursor)?;
    call_ctx.set("query", query)?;
    call_ctx.set(
        "mode",
        match ctx.mode {
            Mode::Insert => "insert",
            _ => "command",
        },
    )?;

    let items = match complete.call::<mlua::Value>(call_ctx)? {
        mlua::Value::Table(list) => decode_lua_items(&list)?,
        _ => Vec::new(),
    };
    if items.is_empty() {
        return Ok(None);
    }
    Ok(Some(((start, end), items)))
}

/// Parses a spec's `trigger` table: `{ kind = 'sigil', char = ':', min_chars
/// = 2 }` or `{ kind = 'line_start', min_chars = 1 }`.
fn parse_lua_trigger(trigger: &mlua::Table) -> mlua::Result<Trigger> {
    let min_chars = trigger.get::<Option<usize>>("min_chars")?.unwrap_or(1);
    match trigger.get::<String>("kind")?.as_str() {
        "line_start" => Ok(Trigger::LineStart { min_chars }),
        "sigil" => {
            let ch: String = trigger.get("char")?;
            let ch = ch
                .chars()
                .next()
                .ok_or_else(|| mlua::Error::external("trigger char must be non-empty"))?;
            Ok(Trigger::Sigil { ch, min_chars })
        }
        other => Err(mlua::Error::external(format!(
            "invalid trigger kind: {other}"
        ))),
    }
}

fn decode_lua_items(list: &mlua::Table) -> mlua::Result<Vec<CompletionItem>> {
    let mut items = Vec::new();
    for value in list.sequence_values::<mlua::Value>() {
        match value? {
            mlua::Value::String(s) => {
                let s = s.to_string_lossy().to_string();
                items.push(CompletionItem {
                    label: s.clone(),
                    insert: s,
                });
            }
            mlua::Value::Table(item) => {
                let insert: String = item.get("insert")?;
                let label: Option<String> = item.get("label")?;
                items.push(CompletionItem {
                    label: label.unwrap_or_else(|| insert.clone()),
                    insert,
                });
            }
            _ => {}
        }
        if items.len() >= MAX_ITEMS {
            break;
        }
    }
    Ok(items)
}

/// Fuzzy-ranks every registry command name and alias against `query`. The
/// insert text carries a trailing space when the command takes arguments.
fn command_name_items(query: &str, matcher: &mut Matcher) -> Vec<CompletionItem> {
    let candidates = commands::BUILTIN_COMMANDS.iter().flat_map(|spec| {
        std::iter::once(spec.name)
            .chain(spec.aliases.iter().copied())
            .map(move |name| {
                let insert = if spec.nargs.takes_args() {
                    format!("{name} ")
                } else {
                    name.to_string()
                };
                (
                    name,
                    CompletionItem {
                        label: name.to_string(),
                        insert,
                    },
                )
            })
    });
    fuzzy_rank(matcher, query, candidates)
}

/// Collects the raw candidate names for one argument kind. State-backed kinds
/// return nothing when the query carries no state or focused buffer.
fn arg_kind_names(kind: ArgKind, ctx: &CompletionQuery, lua: &Lua) -> Vec<String> {
    match kind {
        ArgKind::None => Vec::new(),
        ArgKind::Channel => {
            let (Some(state), Some(focused)) = (ctx.state, ctx.focused) else {
                return Vec::new();
            };
            state
                .buffers
                .keys()
                .filter(|id| id.backend == focused.backend && !id.target.is_status())
                .map(|id| id.target.as_str().to_string())
                .collect()
        }
        ArgKind::Nick => {
            let (Some(state), Some(focused)) = (ctx.state, ctx.focused) else {
                return Vec::new();
            };
            let Some(buffer) = state.buffers.get(focused) else {
                return Vec::new();
            };
            buffer
                .members
                .iter()
                .map(|member| member.user.name().to_string())
                .collect()
        }
        ArgKind::Buffer => {
            let Some(state) = ctx.state else {
                return Vec::new();
            };
            state
                .buffers
                .iter()
                .map(|(id, buffer)| state.buffer_label(id, buffer).to_string())
                .collect()
        }
        ArgKind::Choices(choices) => choices.iter().map(|s| s.to_string()).collect(),
        ArgKind::BarStyle => {
            let mut styles =
                tirc_lua::runtime::ui_string_list(lua, "buffer_bar_styles").unwrap_or_default();
            styles.push("reset".to_string());
            styles
        }
    }
}

/// Completes emoji shortcodes after a `:` sigil in Insert mode, backed by the
/// `emojis` crate's gemoji data. The accepted text is the emoji itself plus a
/// trailing space, Discord-style.
struct EmojiSource;

impl CompletionSource for EmojiSource {
    fn modes(&self) -> &[Mode] {
        &[Mode::Insert]
    }

    fn trigger(&self) -> Trigger {
        Trigger::Sigil {
            ch: ':',
            min_chars: 2,
        }
    }

    fn complete(&self, query: &str, matcher: &mut Matcher) -> Vec<CompletionItem> {
        // One candidate per emoji: its best-scoring shortcode, so a query never
        // shows the same emoji twice under different aliases.
        let candidates = emojis::iter().map(|emoji| {
            let shortcode = emoji.shortcode().unwrap_or_default();
            (
                shortcode,
                CompletionItem {
                    label: format!("{} :{}:", emoji.as_str(), shortcode),
                    insert: format!("{} ", emoji.as_str()),
                },
            )
        });
        fuzzy_rank(matcher, query, candidates)
    }
}

/// Fuzzy-ranks `(match_text, item)` candidates against `query`: matches are
/// sorted by score (best first), ties broken by match text, capped at
/// [`MAX_ITEMS`]. An empty query returns all candidates in order (the Tab
/// force-open path).
fn fuzzy_rank<'a, I>(matcher: &mut Matcher, query: &str, candidates: I) -> Vec<CompletionItem>
where
    I: Iterator<Item = (&'a str, CompletionItem)>,
{
    if query.is_empty() {
        return candidates.take(MAX_ITEMS).map(|(_, item)| item).collect();
    }

    let atom = Atom::new(
        query,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
        false,
    );
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, &str, CompletionItem)> = candidates
        .filter_map(|(text, item)| {
            let score = atom.score(Utf32Str::new(text, &mut buf), matcher)?;
            Some((u32::from(score), text, item))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored
        .into_iter()
        .take(MAX_ITEMS)
        .map(|(_, _, item)| item)
        .collect()
}

/// Finds the sigil span ending at `cursor`: the nearest `sigil` scanning
/// backward with no whitespace (or second sigil) in between, required at index
/// 0 or right after whitespace so `10:30` never triggers. Returns the char
/// range covering the sigil through the cursor plus the query between them
/// (which must have at least `min_chars` chars).
pub fn sigil_span(
    value: &str,
    cursor: usize,
    sigil: char,
    min_chars: usize,
) -> Option<(usize, usize, String)> {
    let chars: Vec<char> = value.chars().collect();
    let cursor = cursor.min(chars.len());
    for i in (0..cursor).rev() {
        let c = chars[i];
        if c.is_whitespace() {
            return None;
        }
        if c == sigil {
            if i > 0 && !chars[i - 1].is_whitespace() {
                return None;
            }
            let query: String = chars[i + 1..cursor].iter().collect();
            if query.chars().count() < min_chars {
                return None;
            }
            return Some((i, cursor, query));
        }
    }
    None
}

/// Finds the first-word span while the cursor is inside it (used for command
/// names). The span covers the whole first word so accepting replaces it even
/// when the cursor sits mid-word; the query is only the part before the
/// cursor.
pub fn line_start_span(
    value: &str,
    cursor: usize,
    min_chars: usize,
) -> Option<(usize, usize, String)> {
    let chars: Vec<char> = value.chars().collect();
    let cursor = cursor.min(chars.len());
    let first_word_len = chars
        .iter()
        .position(|c| c.is_whitespace())
        .unwrap_or(chars.len());
    if cursor > first_word_len {
        return None;
    }
    let query: String = chars[..cursor].iter().collect();
    if query.chars().count() < min_chars {
        return None;
    }
    Some((0, first_word_len, query))
}

/// Detects a just-typed closing `:` completing an exact emoji shortcode, e.g.
/// the final colon of `:smile:`. Returns the char range covering both colons
/// and the emoji text (plus trailing space) to splice in, or `None` when the
/// char before the cursor is not a closing colon or the shortcode is unknown.
pub fn closing_sigil_accept(value: &str, cursor: usize) -> Option<((usize, usize), String)> {
    let chars: Vec<char> = value.chars().collect();
    let cursor = cursor.min(chars.len());
    if cursor == 0 || chars[cursor - 1] != ':' {
        return None;
    }
    let (start, _, query) = sigil_span(value, cursor - 1, ':', 1)?;
    let emoji = emojis::get_by_shortcode(&query)?;
    Some(((start, cursor), format!("{} ", emoji.as_str())))
}

/// Replaces the char range `span` in `value` with `insert`, returning the new
/// value and the char index just past the inserted text (where the cursor
/// belongs).
pub fn splice(value: &str, span: (usize, usize), insert: &str) -> (String, usize) {
    let start = char_to_byte(value, span.0);
    let end = char_to_byte(value, span.1);
    let new_value = format!("{}{}{}", &value[..start], insert, &value[end..]);
    (new_value, span.0 + insert.chars().count())
}

/// The byte offset of the char at `char_idx`, or the string's length when the
/// index is at/past the end.
fn char_to_byte(value: &str, char_idx: usize) -> usize {
    value
        .char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(value.len())
}

/// The completion popup shown above the input line. Plain data on
/// [`ViewState`](super::state::ViewState), following the `ContextMenu`
/// pattern: the input handler mutates it, the renderer draws it and writes
/// back the resolved [`rect`](Self::rect).
#[derive(Debug, Default)]
pub struct CompletionPopup {
    /// Whether the popup is currently shown.
    pub open: bool,
    pub items: Vec<CompletionItem>,
    /// Index of the highlighted item.
    pub selected: usize,
    /// Char range in the input value the accepted item replaces (includes the
    /// sigil for sigil triggers).
    pub span: (usize, usize),
    /// Resolved on-screen rectangle from the most recent render; meaningless
    /// while closed.
    pub rect: Rect,
}

impl CompletionPopup {
    /// Replaces the popup contents with a fresh query result, resetting the
    /// highlight to the first item.
    pub fn show(&mut self, span: (usize, usize), items: Vec<CompletionItem>) {
        self.open = true;
        self.items = items;
        self.selected = 0;
        self.span = span;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.items.clear();
        self.selected = 0;
    }

    /// Moves the highlight up one row, wrapping to the bottom (Tab-cycling).
    pub fn move_up(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = self.selected.checked_sub(1).unwrap_or(self.items.len() - 1);
    }

    /// Moves the highlight down one row, wrapping to the top.
    pub fn move_down(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.items.len();
    }

    pub fn selected_item(&self) -> Option<&CompletionItem> {
        self.items.get(self.selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tirc_core::backend::BackendInfo;
    use tirc_core::{
        BackendId, ChatEvent, MemberRole, MembershipChange, MessageBody, MsgKind, Protocol,
        TargetId, UserRef,
    };

    fn command_query<'a>(value: &'a str, cursor: usize, force: bool) -> CompletionQuery<'a> {
        CompletionQuery {
            mode: Mode::Command,
            value,
            cursor,
            force,
            state: None,
            focused: None,
        }
    }

    /// Backend 0 with #rust (members alice, albert, bob) and #news; backend 1
    /// with #other. Returns the state and backend 0's #rust buffer id.
    fn arg_state() -> (State, BufferId) {
        let mut state = State::new();
        for (id, channels) in [
            (BackendId(0), &["#rust", "#news"][..]),
            (BackendId(1), &["#other"][..]),
        ] {
            state.register_backend(BackendInfo {
                id,
                protocol: Protocol::Irc,
                name: format!("test{}", id.0),
            });
            for channel in channels {
                state.apply(
                    id,
                    ChatEvent::Message {
                        target: TargetId::from(*channel),
                        id: None,
                        sender: UserRef::new("alice"),
                        body: MessageBody::plain("hi"),
                        kind: MsgKind::Text,
                        echo_of: None,
                        time: None,
                    },
                );
            }
        }
        for nick in ["alice", "albert", "bob"] {
            state.apply(
                BackendId(0),
                ChatEvent::Membership {
                    target: TargetId::from("#rust"),
                    who: UserRef::new(nick),
                    change: MembershipChange::Present {
                        role: MemberRole::Member,
                    },
                    time: None,
                },
            );
        }
        (state, BufferId::new(BackendId(0), "#rust"))
    }

    fn labels(items: &[CompletionItem]) -> Vec<&str> {
        items.iter().map(|i| i.label.as_str()).collect()
    }

    #[test]
    fn test_sigil_span_at_line_start() {
        assert_eq!(
            sigil_span(":smi", 4, ':', 2),
            Some((0, 4, "smi".to_string()))
        );
    }

    #[test]
    fn test_sigil_span_after_space() {
        assert_eq!(
            sigil_span("hello :fir", 10, ':', 2),
            Some((6, 10, "fir".to_string()))
        );
    }

    #[test]
    fn test_sigil_span_rejects_mid_word() {
        assert_eq!(sigil_span("see you at 10:30", 16, ':', 2), None);
    }

    #[test]
    fn test_sigil_span_below_min_chars() {
        assert_eq!(sigil_span(":s", 2, ':', 2), None);
        assert_eq!(sigil_span(":s", 2, ':', 1), Some((0, 2, "s".to_string())));
    }

    #[test]
    fn test_sigil_span_whitespace_in_span_closes() {
        assert_eq!(sigil_span(":smi le", 7, ':', 2), None);
    }

    #[test]
    fn test_sigil_span_second_sigil_wins() {
        // The nearer sigil is mid-word, so the span is rejected rather than
        // silently reaching back to the first one.
        assert_eq!(sigil_span(":ab:cd", 6, ':', 2), None);
    }

    #[test]
    fn test_sigil_span_cursor_before_sigil() {
        assert_eq!(sigil_span("ab :cd", 2, ':', 2), None);
    }

    #[test]
    fn test_sigil_span_multibyte_prefix() {
        // 4 chars before the sigil, all multibyte.
        assert_eq!(
            sigil_span("héllö :fir", 10, ':', 2),
            Some((6, 10, "fir".to_string()))
        );
    }

    #[test]
    fn test_line_start_span_inside_first_word() {
        assert_eq!(
            line_start_span("join", 2, 1),
            Some((0, 4, "jo".to_string()))
        );
    }

    #[test]
    fn test_line_start_span_cursor_past_first_word() {
        assert_eq!(line_start_span("join #rust", 6, 1), None);
    }

    #[test]
    fn test_line_start_span_empty_input() {
        assert_eq!(line_start_span("", 0, 1), None);
        assert_eq!(line_start_span("", 0, 0), Some((0, 0, String::new())));
    }

    #[test]
    fn test_closing_sigil_accept_exact_shortcode() {
        let (span, insert) = closing_sigil_accept("hi :smile:", 10).unwrap();
        assert_eq!(span, (3, 10));
        assert_eq!(
            insert.trim_end(),
            emojis::get_by_shortcode("smile").unwrap().as_str()
        );
    }

    #[test]
    fn test_closing_sigil_accept_unknown_shortcode() {
        assert_eq!(closing_sigil_accept("hi :nope-not-real:", 18), None);
    }

    #[test]
    fn test_closing_sigil_accept_requires_trailing_colon() {
        assert_eq!(closing_sigil_accept("hi :smile", 9), None);
    }

    #[test]
    fn test_splice_mid_string() {
        let (value, cursor) = splice("say :smi now", (4, 8), "😄 ");
        assert_eq!(value, "say 😄  now");
        assert_eq!(cursor, 6);
    }

    #[test]
    fn test_splice_with_multibyte_prefix() {
        let (value, cursor) = splice("🔥 :smi", (2, 6), "😄 ");
        assert_eq!(value, "🔥 😄 ");
        assert_eq!(cursor, 4);
    }

    #[test]
    fn test_fuzzy_rank_prefers_better_score() {
        let mut matcher = Matcher::default();
        let candidates = ["campfire", "fire", "firecracker"].map(|name| {
            (
                name,
                CompletionItem {
                    label: name.to_string(),
                    insert: name.to_string(),
                },
            )
        });
        let ranked = fuzzy_rank(&mut matcher, "fire", candidates.into_iter());
        assert_eq!(ranked[0].label, "fire");
        assert_eq!(ranked.len(), 3);
    }

    #[test]
    fn test_fuzzy_rank_empty_query_returns_all() {
        let mut matcher = Matcher::default();
        let candidates = ["a", "b"].map(|name| {
            (
                name,
                CompletionItem {
                    label: name.to_string(),
                    insert: name.to_string(),
                },
            )
        });
        let ranked = fuzzy_rank(&mut matcher, "", candidates.into_iter());
        assert_eq!(ranked.len(), 2);
    }

    #[test]
    fn test_fuzzy_rank_caps_results() {
        let mut matcher = Matcher::default();
        let names: Vec<String> = (0..100).map(|i| format!("item{i}")).collect();
        let candidates = names.iter().map(|name| {
            (
                name.as_str(),
                CompletionItem {
                    label: name.clone(),
                    insert: name.clone(),
                },
            )
        });
        let ranked = fuzzy_rank(&mut matcher, "item", candidates);
        assert_eq!(ranked.len(), MAX_ITEMS);
    }

    #[test]
    fn test_command_name_items_trailing_space() {
        let mut matcher = Matcher::default();
        let items = command_name_items("jo", &mut matcher);
        let join = items.iter().find(|i| i.label == "join").unwrap();
        assert_eq!(join.insert, "join ");
    }

    #[test]
    fn test_command_name_items_short_aliases() {
        let mut matcher = Matcher::default();
        let items = command_name_items("q", &mut matcher);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"q"));
        assert!(labels.contains(&"quit"));
        // quit takes no arguments, so accepting it must not add a space.
        let quit = items.iter().find(|i| i.label == "quit").unwrap();
        assert_eq!(quit.insert, "quit");
    }

    #[test]
    fn test_command_name_query_only_inside_first_word() {
        let mut engine = CompletionEngine::new();
        let ctx = command_query("jo", 2, false);
        let ((start, end), items) = engine.command_name_query(&ctx).unwrap();
        assert_eq!((start, end), (0, 2));
        assert!(items.iter().any(|i| i.label == "join"));

        let ctx = command_query("join #rust", 7, false);
        assert_eq!(engine.command_name_query(&ctx), None);
    }

    #[test]
    fn test_arg_completion_channels_scoped_to_focused_backend() {
        let (state, focused) = arg_state();
        let lua = Lua::new();
        let mut engine = CompletionEngine::new();

        let mut ctx = command_query("join ", 5, true);
        ctx.state = Some(&state);
        ctx.focused = Some(&focused);
        let ((start, end), items) = engine.command_arg_query(&ctx, &lua).unwrap();
        assert_eq!((start, end), (5, 5));
        let labels = labels(&items);
        assert!(labels.contains(&"#rust"));
        assert!(labels.contains(&"#news"));
        assert!(!labels.contains(&"#other"), "other backend's channels leak");
        assert!(!labels.contains(&TargetId::STATUS), "status buffer leaks");
    }

    #[test]
    fn test_arg_completion_final_position_no_trailing_space() {
        let (state, focused) = arg_state();
        let lua = Lua::new();
        let mut engine = CompletionEngine::new();

        let mut ctx = command_query("join #r", 7, false);
        ctx.state = Some(&state);
        ctx.focused = Some(&focused);
        let (span, items) = engine.command_arg_query(&ctx, &lua).unwrap();
        assert_eq!(span, (5, 7));
        assert_eq!(items[0].insert, "#rust");
    }

    #[test]
    fn test_arg_completion_nicks_with_trailing_space() {
        let (state, focused) = arg_state();
        let lua = Lua::new();
        let mut engine = CompletionEngine::new();

        // msg's target is not the final declared position, so accepting a
        // nick moves on to the message with a trailing space.
        let mut ctx = command_query("msg al", 6, false);
        ctx.state = Some(&state);
        ctx.focused = Some(&focused);
        let (_, items) = engine.command_arg_query(&ctx, &lua).unwrap();
        let labels = labels(&items);
        assert!(labels.contains(&"alice"));
        assert!(labels.contains(&"albert"));
        assert!(!labels.contains(&"bob"));
        assert!(items.iter().all(|i| i.insert.ends_with(' ')));
    }

    #[test]
    fn test_arg_completion_prefix_resolved_command() {
        let (state, focused) = arg_state();
        let lua = Lua::new();
        let mut engine = CompletionEngine::new();

        // "jo" prefix-resolves to join, so its channel argument completes.
        let mut ctx = command_query("jo #n", 5, false);
        ctx.state = Some(&state);
        ctx.focused = Some(&focused);
        let (_, items) = engine.command_arg_query(&ctx, &lua).unwrap();
        assert_eq!(items[0].label, "#news");
    }

    #[test]
    fn test_arg_completion_choices() {
        let lua = Lua::new();
        let mut engine = CompletionEngine::new();

        let ctx = command_query("verify acc", 10, false);
        let (_, items) = engine.command_arg_query(&ctx, &lua).unwrap();
        assert_eq!(labels(&items), ["accept"]);
    }

    #[test]
    fn test_arg_completion_requires_query_unless_forced() {
        let (state, focused) = arg_state();
        let lua = Lua::new();
        let mut engine = CompletionEngine::new();

        // No query char and no Tab: stay closed so the popup does not open
        // on every space keypress.
        let mut ctx = command_query("join ", 5, false);
        ctx.state = Some(&state);
        ctx.focused = Some(&focused);
        assert_eq!(engine.command_arg_query(&ctx, &lua), None);
    }

    #[test]
    fn test_arg_completion_unknown_command_and_extra_positions() {
        let (state, focused) = arg_state();
        let lua = Lua::new();
        let mut engine = CompletionEngine::new();

        let mut ctx = command_query("xyz #r", 6, false);
        ctx.state = Some(&state);
        ctx.focused = Some(&focused);
        assert_eq!(engine.command_arg_query(&ctx, &lua), None);

        // join declares a single argument position; a second one does not
        // complete.
        let mut ctx = command_query("join #rust #n", 13, false);
        ctx.state = Some(&state);
        ctx.focused = Some(&focused);
        assert_eq!(engine.command_arg_query(&ctx, &lua), None);
    }

    #[test]
    fn test_emoji_source_finds_smile() {
        let mut matcher = Matcher::default();
        let items = EmojiSource.complete("smi", &mut matcher);
        assert!(items.len() <= MAX_ITEMS);
        let smile = items.iter().find(|i| i.label.contains(":smile:")).unwrap();
        assert!(smile.insert.ends_with(' '));
        assert_eq!(
            smile.insert.trim_end(),
            emojis::get_by_shortcode("smile").unwrap().as_str()
        );
    }

    #[test]
    fn test_popup_navigation_wraps() {
        let mut popup = CompletionPopup::default();
        popup.show(
            (0, 2),
            vec![
                CompletionItem {
                    label: "a".into(),
                    insert: "a".into(),
                },
                CompletionItem {
                    label: "b".into(),
                    insert: "b".into(),
                },
            ],
        );
        assert_eq!(popup.selected, 0);
        popup.move_up();
        assert_eq!(popup.selected, 1);
        popup.move_down();
        assert_eq!(popup.selected, 0);
        popup.move_down();
        popup.move_down();
        assert_eq!(popup.selected, 0);
    }
}
