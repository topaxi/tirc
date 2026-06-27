use mlua::LuaSerdeExt;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListDirection, ListItem, Paragraph},
};
use tui_input::Input;

use crate::backends::BackendInfo;
use crate::core::{BufferId, TargetId};
use crate::lua::date_time::date_time_to_table;
use crate::ui::{ChatBuffer, Member, Mode, State, StoredMessage, ViewState};

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

    fn get_layout(&self) -> Layout {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints(
                [
                    Constraint::Min(0),
                    Constraint::Length(2),
                    Constraint::Length(1),
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
        target: &TargetId,
    ) -> Result<Vec<Span<'_>>, anyhow::Error> {
        self.format_spans(
            lua,
            "buffer_title",
            (
                backend.name.clone(),
                nickname.to_string(),
                target.as_str().to_string(),
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

        let messages = buffer
            .messages
            .iter()
            .rev()
            // Render a bit more than fits, as some lines are filtered out and
            // others wrap. TODO: scroll from buffer.scroll_position.
            .take((rect.height as usize) + (rect.height as usize) / 2)
            .filter_map(|message| self.render_message(lua, backend, &buffer_id.target, message))
            .collect::<Vec<_>>();

        let messages = messages
            .iter()
            .filter(|message| message.message.width() > 0)
            .map(|message| {
                let initial_indent = message.time.clone();

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

                wrap_line(
                    &message.message,
                    super::wrap::Options {
                        width: rect.width as usize,
                        initial_indent,
                        subsequent_indent,
                        break_words: true,
                    },
                )
            })
            .map(ListItem::new);

        let title = self
            .render_buffer_title(lua, backend, nickname, &buffer_id.target)
            .unwrap_or_default();

        let list = List::new(messages)
            .block(Block::default().title(title).borders(Borders::NONE))
            .direction(ListDirection::BottomToTop);

        f.render_widget(list, rect);
    }

    fn render_message(
        &self,
        lua: &mlua::Lua,
        backend: &BackendInfo,
        target: &TargetId,
        message: &StoredMessage,
    ) -> Option<RenderedMessage<'_>> {
        let event = to_lua_event(lua, message, backend, target).ok()?;

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

    fn render_buffer_bar(
        &self,
        f: &mut ratatui::Frame,
        state: &State,
        view: &ViewState,
        rect: Rect,
    ) {
        let multi_backend = state.backends.len() > 1;

        let buffers: Vec<Span> = state
            .buffers
            .keys()
            .flat_map(|id| {
                let mut style = Style::default();
                if view.focused.as_ref() == Some(id) {
                    style = style.add_modifier(Modifier::BOLD);
                }

                let label = if multi_backend {
                    let name = state
                        .backends
                        .get(&id.backend)
                        .map(|b| b.info.name.as_str())
                        .unwrap_or("?");
                    format!("{name}/{}", id.target.as_str())
                } else {
                    id.target.as_str().to_string()
                };

                [Span::styled(label, style), Span::raw(" ")]
            })
            .collect();

        f.render_widget(Paragraph::new(Line::from(buffers)), rect);
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
        target: &TargetId,
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

        let list = List::new(users).block(
            Block::default()
                .title(target.as_str().to_string())
                .borders(Borders::LEFT),
        );
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
        view: &ViewState,
        lua: &mlua::Lua,
        input: &Input,
    ) {
        let layout = self.get_layout();
        let chunks = layout.split(f.area());

        let members = self
            .focused(state, view)
            .map(|(id, buffer, _, _)| (id.target.clone(), &buffer.members));

        match members {
            Some((target, members)) if members.len() > 1 => {
                let split = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(90), Constraint::Percentage(10)].as_ref())
                    .split(chunks[0]);

                self.render_users(f, members, lua, &target, split[1]);
                self.render_messages(f, state, view, lua, split[0]);
            }
            _ => self.render_messages(f, state, view, lua, chunks[0]),
        }

        self.render_buffer_bar(f, state, view, chunks[2]);
        self.render_input(f, view, input, chunks[1]);
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
}
