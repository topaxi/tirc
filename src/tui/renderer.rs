use irc::client::data::AccessLevel;
use mlua::LuaSerdeExt;
use tui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListDirection, ListItem, Paragraph},
};
use tui_input::Input;

use crate::{
    config,
    lua::date_time::date_time_to_table,
    ui::{Mode, State, TircMessage},
};

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
    ) -> Result<Vec<Span>, anyhow::Error> {
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
                let span = Self::string_to_span(string, parent_style);

                spans.push(span);
            }
            mlua::Value::Table(v) => {
                // Table of two values might be a styled message
                if v.len()? == 2 {
                    let style = v.get::<_, Option<mlua::Value>>(2)?;
                    let style = if matches!(style, Some(mlua::Value::Table(_))) {
                        lua.from_value::<Style>(style.unwrap())
                            .map(|style| {
                                if let Some(parent_style) = parent_style {
                                    parent_style.patch(style)
                                } else {
                                    style
                                }
                            })
                            .ok()
                    } else {
                        None
                    };

                    if let Some(style) = style {
                        let value = v.get::<_, Option<mlua::Value>>(1)?;

                        if let Some(value) = value {
                            Self::flatten_lua_value(lua, value, spans, Some(style))?;
                        }

                        return Ok(());
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
        if let Some(style) = style {
            Span::styled(str, style)
        } else {
            Span::raw(str)
        }
    }

    fn render_message_time(
        &self,
        lua: &mlua::Lua,
        date_time: &mlua::Table,
        message: &mlua::Table,
    ) -> Result<Vec<Span>, anyhow::Error> {
        let v = config::emit_sync_callback(lua, "format-message-time", (date_time, message))?;

        self.lua_value_to_spans(lua, v)
    }

    fn render_message_text(
        &self,
        lua: &mlua::Lua,
        message: &mlua::Table,
        nickname: &str,
    ) -> Result<Vec<Span>, anyhow::Error> {
        let v = config::emit_sync_callback(lua, "format-message-text", (message, nickname))?;

        self.lua_value_to_spans(lua, v)
    }

    fn render_buffer_title(
        &self,
        lua: &mlua::Lua,
        state: &State,
    ) -> Result<Vec<Span>, anyhow::Error> {
        let v = config::emit_sync_callback(
            lua,
            "format-buffer-title",
            (
                state.server.clone(),
                state.nickname.clone(),
                state.current_buffer.clone(),
            ),
        )?;

        self.lua_value_to_spans(lua, v)
    }

    fn render_messages(&self, f: &mut tui::Frame, state: &State, lua: &mlua::Lua, rect: Rect) {
        let current_buffer_name = &state.current_buffer;
        let buffers = &state.buffers;

        let current_buffer = buffers.get(current_buffer_name).unwrap();

        let messages = current_buffer
            .messages
            .iter()
            .rev()
            // Do not render _all_ messages, only the ones that fit in the available space
            // We render a bit more as some messages might get filtered out. Although some might
            // wrap and make even out the edge case.
            // TODO: Make message list scrollable
            .take((rect.height as usize) + (rect.height as usize) / 2)
            .filter_map(|tirc_message| self.render_message(state, lua, tirc_message))
            .collect::<Vec<_>>();

        let messages = messages
            .iter()
            .filter(|message| !message.message.width() > 0)
            .map(|message| {
                let initial_indent = message.time.clone();

                // TODO: This is a hack to have the time | user separator included in the
                // subsequent indent. It would be better to have a more explicit solution.
                let subsequent_indent = if initial_indent.len() > 0 {
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
            .map(ListItem::new)
            .collect::<Vec<_>>();

        let list = List::new(messages)
            .block(
                Block::default()
                    .title(
                        self.render_buffer_title(lua, state)
                            .unwrap_or_else(|_| vec![]),
                    )
                    .borders(Borders::NONE),
            )
            .direction(ListDirection::BottomToTop);

        f.render_widget(list, rect);
    }

    fn render_message(
        &self,
        state: &State,
        lua: &mlua::Lua,
        tirc_message: &TircMessage,
    ) -> Option<RenderedMessage> {
        if let TircMessage::Irc(date_time, message, lua_message) = tirc_message {
            let mut time_spans = self
                .render_message_time(
                    lua,
                    &date_time_to_table(lua, date_time).unwrap(),
                    lua_message,
                )
                .unwrap_or_else(|_| vec![]);

            if time_spans.len() == 1 {
                time_spans.push(Span::raw(""));
            }

            let message_spans = self
                .render_message_text(lua, lua_message, &state.nickname)
                .unwrap_or_else(|_| vec![Span::raw(message.to_string())]);

            if message_spans.is_empty() {
                return None;
            }

            Some(RenderedMessage {
                time: time_spans.into_boxed_slice(),
                message: Box::new(Line::from(message_spans)),
            })
        } else {
            None
        }
    }

    fn render_buffer_bar(&self, f: &mut tui::Frame, state: &State, rect: Rect) {
        let current_buffer_name = &state.current_buffer;

        let buffers: Vec<Span> = state
            .buffers
            .keys()
            .flat_map(|str| {
                let mut style = Style::default();

                if str == current_buffer_name {
                    style = style.add_modifier(Modifier::BOLD);
                }

                [Span::styled(str.to_owned(), style), Span::raw(" ")]
            })
            .collect();

        let buffer_bar = Paragraph::new(Line::from(buffers));

        f.render_widget(buffer_bar, rect);
    }

    fn render_input(&mut self, f: &mut tui::Frame, state: &State, input: &Input, rect: Rect) {
        let prefix = match state.mode {
            Mode::Normal => "",
            Mode::Command => ":",
            Mode::Insert => "❯ ",
        };
        let prefix_len = prefix.chars().count() as u16;
        let width = f.area().width.max(3) - prefix_len; // keep 2 for borders and 1 for cursor
        let scroll = input.visual_scroll(width as usize);
        let p = Paragraph::new(format!("{}{}", prefix, input.value()))
            .scroll((0, scroll as u16))
            .block(Block::default().borders(Borders::TOP));
        f.render_widget(p, rect);

        match state.mode {
            Mode::Normal => {}

            Mode::Command | Mode::Insert => {
                // Make the cursor visible and ask tui-rs to put it at the specified coordinates after rendering
                f.set_cursor_position((
                    // Put cursor past the end of the input text
                    rect.x + ((input.visual_cursor()).max(scroll) - scroll) as u16 + prefix_len,
                    // Move one line down, from the border to the input line
                    rect.y + 1,
                ))
            }
        }
    }

    fn render_user(&self, lua: &mlua::Lua, user: &mlua::Table) -> Result<Vec<Span>, anyhow::Error> {
        let v = config::emit_sync_callback(lua, "format-user", user)?;

        self.lua_value_to_spans(lua, v)
    }

    fn get_access_level_priority(access_level: &AccessLevel) -> i32 {
        match access_level {
            AccessLevel::Owner => 0,
            AccessLevel::Admin => 1,
            AccessLevel::Oper => 2,
            AccessLevel::HalfOp => 3,
            AccessLevel::Voice => 4,
            AccessLevel::Member => 5,
        }
    }

    fn render_users(&self, f: &mut tui::Frame, state: &State, lua: &mlua::Lua, rect: Rect) {
        let mut users = state.users_in_current_buffer.to_vec();

        // TODO: We might not want to sort the users every time we render the user list.
        //       A good way might be to hold the users in a sorted datastructure on the state
        //       itself.
        users.sort_unstable_by(|a, b| {
            Self::get_access_level_priority(&a.highest_access_level())
                .cmp(&Self::get_access_level_priority(&b.highest_access_level()))
                .then_with(|| a.get_nickname().cmp(b.get_nickname()))
        });

        let users = users
            .iter()
            // TODO: Make user list scrollable
            .take(rect.height as usize)
            .map(|user| {
                let lua_user = lua.to_value(user);
                let rendered_user = if let Ok(mlua::Value::Table(tbl)) = lua_user {
                    self.render_user(lua, &tbl).unwrap_or_default()
                } else {
                    vec![]
                };

                if rendered_user.is_empty() {
                    return ListItem::new(user.get_nickname());
                }

                ListItem::new(Line::from(rendered_user))
            })
            .collect::<Vec<_>>();
        let list = List::new(users).block(
            Block::default()
                .title(state.current_buffer.to_owned())
                .borders(Borders::LEFT),
        );
        f.render_widget(list, rect);
    }

    pub fn render(&mut self, f: &mut tui::Frame, state: &State, lua: &mlua::Lua, input: &Input) {
        let layout = self.get_layout();
        let size = f.area();
        let chunks = layout.split(size);

        if state.users_in_current_buffer.len() > 1 {
            let layout_with_sidebar = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(90), Constraint::Percentage(10)].as_ref())
                .split(chunks[0]);

            self.render_users(f, state, lua, layout_with_sidebar[1]);
            self.render_messages(f, state, lua, layout_with_sidebar[0]);
        } else {
            self.render_messages(f, state, lua, chunks[0]);
        }

        self.render_buffer_bar(f, state, chunks[2]);
        self.render_input(f, state, input, chunks[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use tui::style::Color;
    use tui::text::Span;

    use crate::tui::lua::create_tirc_theme_lua_module;

    fn run_lua_code<'lua>(
        lua: &'lua mlua::Lua,
        code: &'lua str,
    ) -> mlua::Result<mlua::Value<'lua>> {
        lua.load(code).eval()
    }

    fn render_lua_table_to_spans<'lua>(
        lua: &'lua mlua::Lua,
        renderer: &'lua Renderer,
        table: &'lua str,
    ) -> Result<Vec<Span<'lua>>, anyhow::Error> {
        let value = run_lua_code(lua, table)?;
        let spans = renderer.lua_value_to_spans(lua, value)?;
        Ok(spans)
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
        assert_eq!(spans[0].content, "Hello");
        assert_eq!(spans[1].content, ", ");
        assert_eq!(spans[2].content, "World!");
        Ok(())
    }

    #[test]
    fn test_lua_value_to_spans_deeply_nested() -> anyhow::Result<(), anyhow::Error> {
        let renderer = Renderer::new();
        let lua = mlua::Lua::new();
        let spans = render_lua_table_to_spans(
            &lua,
            &renderer,
            indoc! {"
                { 'a', { 'b', 'c', { 'd', 'e' } }, 'f' }
            "},
        )?;
        assert_eq!(spans.len(), 6);
        assert_eq!(spans[0].content, "a");
        assert_eq!(spans[1].content, "b");
        assert_eq!(spans[2].content, "c");
        assert_eq!(spans[3].content, "d");
        assert_eq!(spans[4].content, "e");
        assert_eq!(spans[5].content, "f");
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
        assert_eq!(spans[0].style.bg, None);
        assert_eq!(spans[1].content, "b");
        assert_eq!(spans[1].style.fg, Some(Color::DarkGray));
        assert_eq!(spans[1].style.bg, Some(Color::White));
        assert_eq!(spans[2].content, "c");
        assert_eq!(spans[2].style.fg, Some(Color::DarkGray));
        assert_eq!(spans[2].style.bg, Some(Color::White));
        assert_eq!(spans[3].content, "d");
        assert_eq!(spans[3].style.fg, Some(Color::Green));
        assert_eq!(spans[3].style.bg, Some(Color::White));
        assert_eq!(spans[4].content, "e");
        assert_eq!(spans[4].style.fg, Some(Color::DarkGray));
        assert_eq!(spans[4].style.bg, Some(Color::White));
        assert_eq!(spans[5].content, "f");
        assert_eq!(spans[5].style.fg, None);
        assert_eq!(spans[5].style.bg, None);
        Ok(())
    }
}
