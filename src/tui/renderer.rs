use mlua::LuaSerdeExt;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListDirection, ListItem, Paragraph},
};
use tui_input::Input;

use crate::backends::BackendInfo;
use crate::core::{BufferId, TargetId};
use crate::lua::date_time::date_time_to_table;
use crate::ui::{ChatBuffer, LayoutMap, Member, Mode, State, StoredMessage, ViewState};

use super::lua::{to_lua_event, to_lua_user, STYLE_MARKER};
use super::wrap::wrap_line;

#[derive(Debug)]
pub struct Renderer {}

#[derive(Debug, Clone, Default)]
pub struct RenderedMessage<'a> {
    pub time: Box<[Span<'a>]>,
    pub message: Box<Line<'a>>,
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

impl Renderer {
    pub fn new() -> Self {
        Self {}
    }

    fn get_layout(&self, bar_height: u16) -> Layout {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints(
                [
                    Constraint::Min(0),
                    Constraint::Length(2),
                    Constraint::Length(bar_height),
                ]
                .as_ref(),
            )
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

    fn render_messages(
        &self,
        f: &mut ratatui::Frame,
        state: &State,
        view: &ViewState,
        lua: &mlua::Lua,
        rect: Rect,
    ) {
        let Some((buffer_id, buffer, backend, nickname)) = self.focused(state, view) else {
            return;
        };

        let target_name = buffer.label(&buffer_id.target);
        let total = buffer.messages.len();
        // Clamp scroll so we always render at least the oldest message when any exist.
        let scroll = buffer.scroll_position.min(total.saturating_sub(1));

        // Collect (RenderedMessage, message_time) pairs so we can detect the
        // read boundary and inject a separator between read and unread messages.
        let rendered: Vec<(RenderedMessage, chrono::DateTime<chrono::Local>)> = buffer
            .messages
            .iter()
            .rev()
            .skip(scroll)
            // Render a bit more than fits, as some lines are filtered out and
            // others wrap.
            .take((rect.height as usize) + (rect.height as usize) / 2)
            .filter_map(|message| {
                self.render_message(lua, backend, &buffer_id.target, target_name, message)
                    .map(|rm| (rm, message.time))
            })
            .collect();

        let read_marker = buffer.read_marker;
        let mut seen_unread = false;
        let mut separator_inserted = false;
        let mut messages: Vec<ListItem<'_>> = Vec::with_capacity(rendered.len() + 1);

        for (rm, msg_time) in &rendered {
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
                        messages.push(ListItem::new(self.render_unread_separator(lua)));
                        separator_inserted = true;
                    }
                }
            }

            let initial_indent = rm.time.clone();

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

            messages.push(ListItem::new(wrap_line(
                &rm.message,
                super::wrap::Options {
                    width: rect.width as usize,
                    initial_indent,
                    subsequent_indent,
                    break_words: true,
                },
            )));
        }

        let title = self
            .render_buffer_title(lua, backend, nickname, buffer.label(&buffer_id.target))
            .unwrap_or_default();

        let list = List::new(messages)
            .block(Block::default().title(title).borders(Borders::NONE))
            .direction(ListDirection::BottomToTop);

        f.render_widget(list, rect);
    }

    /// Renders the "new messages" separator line. The appearance is driven by
    /// the theme's `render_unread_separator` formatter; falls back to a plain
    /// styled line when the theme does not implement it.
    fn render_unread_separator(&self, lua: &mlua::Lua) -> Line<'_> {
        match self.format_spans(lua, "render_unread_separator", ()) {
            Ok(spans) if !spans.is_empty() => Line::from(spans),
            _ => Line::from(Span::styled(
                "─── new messages ───",
                Style::default().fg(Color::DarkGray),
            )),
        }
    }

    fn render_message(
        &self,
        lua: &mlua::Lua,
        backend: &BackendInfo,
        target: &TargetId,
        target_name: &str,
        message: &StoredMessage,
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

        Some(RenderedMessage {
            time: time_spans.into_boxed_slice(),
            message: Box::new(Line::from(message_spans)),
        })
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
        t.set("backend_id", id.backend.0)?;
        t.set("backend_name", backend_name)?;
        if let Some(metadata) = crate::config::get_backend_metadata(lua, id.backend) {
            t.set("backend_metadata", metadata)?;
        }
        t.set("has_unread", buffer.has_unread)?;
        t.set("has_mention", buffer.has_mention)?;
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

    /// Measures each buffer's rendered tab and accumulates left-to-right hit
    /// boxes along the first row of the bar. Tabs are measured in `state.buffers`
    /// order using the same per-buffer `render_buffer_tab` formatter the default
    /// theme concatenates, so the boxes line up with the drawn bar exactly. This
    /// must run after `update_render_context`, because a tab's width can depend
    /// on the `_tirc` globals that context populates (e.g. `has_unique_name`).
    ///
    /// The trailing separator a theme appends to a tab is attributed to that
    /// tab, leaving the bar contiguous with no dead zones between tabs.
    fn build_bar_tabs(
        &self,
        state: &State,
        lua: &mlua::Lua,
        bar_rect: Rect,
    ) -> Vec<(Rect, BufferId)> {
        let mut tabs = Vec::with_capacity(state.buffers.len());
        let mut x = bar_rect.x;

        for (id, buffer) in state.buffers.iter() {
            let width: u16 = self
                .buffer_tab_table(state, lua, id, buffer)
                .ok()
                .and_then(|tab| self.format_spans(lua, "render_buffer_tab", tab).ok())
                .map(|spans| spans.iter().map(|span| span.width() as u16).sum())
                .unwrap_or(0);

            if width == 0 {
                continue;
            }

            tabs.push((
                Rect {
                    x,
                    y: bar_rect.y,
                    width,
                    height: 1,
                },
                id.clone(),
            ));
            x = x.saturating_add(width);
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

    /// Converts a `render_buffer_bar` result into rendered lines. A table with a
    /// `rows` sequence yields one line per row; any other value is treated as a
    /// single row (the shorthand documented for `render_buffer_bar`).
    fn lua_value_to_rows(
        &self,
        lua: &mlua::Lua,
        value: mlua::Value,
    ) -> Result<Vec<Line<'_>>, anyhow::Error> {
        if let mlua::Value::Table(table) = &value {
            if let mlua::Value::Table(rows) = table.get::<mlua::Value>("rows")? {
                return rows
                    .sequence_values::<mlua::Value>()
                    .map(|row| Ok(Line::from(self.lua_value_to_spans(lua, row?)?)))
                    .collect();
            }
        }

        Ok(vec![Line::from(self.lua_value_to_spans(lua, value)?)])
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

    /// Produces the buffer bar as a list of lines plus a base background style.
    /// Delegates the whole layout to the theme's `render_buffer_bar`; when that
    /// formatter is absent, falls back to a single line built from per-tab
    /// `render_buffer_tab` results so raw `TircUi` themes keep working.
    fn build_buffer_bar(&self, state: &State, lua: &mlua::Lua) -> (Vec<Line<'_>>, Style) {
        let tabs = match self.buffer_tabs(state, lua) {
            Ok(tabs) => tabs,
            Err(_) => return (vec![Line::default()], Style::default()),
        };

        match crate::config::call_formatter(lua, "render_buffer_bar", &tabs) {
            Some(Ok(mlua::Value::Table(table))) => {
                let bg_style = Self::bar_bg_style(&table);
                let lines = self
                    .lua_value_to_rows(lua, mlua::Value::Table(table))
                    .unwrap_or_default();
                (lines, bg_style)
            }
            Some(Ok(value)) => {
                let lines = self.lua_value_to_rows(lua, value).unwrap_or_default();
                (lines, Style::default())
            }
            Some(Err(err)) => (
                vec![Line::from(Self::string_to_span(
                    format!("ERR: {err}"),
                    Some(Style::default().fg(Color::Red)),
                ))],
                Style::default(),
            ),
            None => {
                let spans = tabs
                    .sequence_values::<mlua::Table>()
                    .filter_map(Result::ok)
                    .flat_map(|tab| {
                        self.format_spans(lua, "render_buffer_tab", tab)
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>();
                (vec![Line::from(spans)], Style::default())
            }
        }
    }

    fn render_input(
        &mut self,
        f: &mut ratatui::Frame,
        view: &ViewState,
        input: &Input,
        rect: Rect,
    ) {
        let prefix = match view.mode {
            Mode::Normal => "",
            Mode::Command => ":",
            Mode::Insert => "❯ ",
        };
        let prefix_len = prefix.chars().count() as u16;
        let width = f.area().width.max(3) - prefix_len;
        let scroll = input.visual_scroll(width as usize);
        let p = Paragraph::new(format!("{}{}", prefix, input.value()))
            .scroll((0, scroll as u16))
            .block(Block::default().borders(Borders::TOP));
        f.render_widget(p, rect);

        match view.mode {
            Mode::Normal => {}
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
        let (bar_lines, bar_style) = self.build_buffer_bar(state, lua);
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
                let split = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(90), Constraint::Percentage(10)].as_ref())
                    .split(chunks[0]);

                self.render_users(f, members, lua, &title, split[1]);
                userlist_rect = Some(split[1]);
                split_x = Some(split[1].x);
                split[0]
            }
            _ => chunks[0],
        };

        view.viewport_height = msg_rect.height;

        self.render_messages(f, state, view, lua, msg_rect);

        f.render_widget(
            Paragraph::new(Text::from(bar_lines)).style(bar_style),
            chunks[2],
        );
        self.render_input(f, view, input, chunks[1]);

        // Record this frame's hit regions so the input handler can resolve mouse
        // clicks without re-deriving the layout. Built last, after the bar's Lua
        // context is in place, so the tab widths match what was drawn.
        let bar_tabs = self.build_bar_tabs(state, lua, chunks[2]);
        view.layout = LayoutMap {
            message_rect: msg_rect,
            bar_rect: chunks[2],
            bar_tabs,
            userlist_rect,
            userlist_first_member: 0,
            split_x,
        };
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
        let rows = renderer.lua_value_to_rows(&lua, value)?;
        assert_eq!(rows.len(), 2);
        Ok(())
    }

    #[test]
    fn lua_value_to_rows_treats_bare_value_as_single_row() -> anyhow::Result<(), anyhow::Error> {
        let renderer = Renderer::new();
        let lua = mlua::Lua::new();
        let value = run_lua_code(&lua, "{ 'x', 'y' }")?;
        let rows = renderer.lua_value_to_rows(&lua, value)?;
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
        let tabs = renderer.build_bar_tabs(&state, &lua, bar_rect);

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
        let rows = renderer.lua_value_to_rows(&lua, value)?;

        assert_eq!(rows.len(), 1);
        let text: String = rows[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("#tirc"), "row text was {text:?}");
        Ok(())
    }
}
