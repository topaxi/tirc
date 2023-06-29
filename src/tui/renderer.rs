use std::io::Stdout;

use chrono::{DateTime, Datelike, Timelike};
use irc::proto::Message;
use mlua::LuaSerdeExt;
use tui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
};
use tui_input::Input;

use crate::{
    config,
    ui::{Mode, State},
};

use super::lua::to_lua_message;

pub struct Renderer {}

impl Renderer {
    pub fn new() -> Renderer {
        Renderer {}
    }

    fn get_layout(&self) -> Layout {
        return Layout::default()
            .direction(Direction::Vertical)
            .constraints(
                [
                    Constraint::Min(0),
                    Constraint::Length(1),
                    Constraint::Length(2),
                ]
                .as_ref(),
            );
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
                spans.push(Self::string_to_span(str.to_str()?.to_owned(), parent_style));
            }
            mlua::Value::Table(v) => {
                for v in v.sequence_values::<mlua::Value>() {
                    let v = v?;
                    match v {
                        mlua::Value::Table(v) => {
                            let style = v.get::<_, Option<mlua::Value>>(2)?;
                            let style = match style {
                                Some(mlua::Value::Table(_)) => {
                                    match lua.from_value::<Style>(style.unwrap()) {
                                        Ok(style) => {
                                            // Apply parent style onto this style
                                            let style = if let Some(parent_style) = parent_style {
                                                parent_style.patch(style)
                                            } else {
                                                style
                                            };
                                            Some(style)
                                        }
                                        _ => None,
                                    }
                                }
                                _ => None,
                            };

                            if let Some(style) = style {
                                let value = v.get::<_, Option<mlua::Value>>(1)?;

                                if let Some(value) = value {
                                    Self::flatten_lua_value(lua, value, spans, Some(style))?;
                                }
                            } else {
                                Self::flatten_lua_value(
                                    lua,
                                    mlua::Value::Table(v),
                                    spans,
                                    parent_style,
                                )?;
                            }
                        }
                        _ => {
                            Self::flatten_lua_value(lua, v, spans, parent_style)?;
                        }
                    }
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

    fn date_time_to_table<'a>(
        &'a self,
        lua: &'a mlua::Lua,
        date_time: &DateTime<chrono::Local>,
    ) -> mlua::Result<mlua::Table> {
        let table = lua.create_table()?;
        table.set("year", date_time.year())?;
        table.set("month", date_time.month())?;
        table.set("day", date_time.day())?;
        table.set("hour", date_time.hour())?;
        table.set("minute", date_time.minute())?;
        table.set("second", date_time.second())?;
        Ok(table)
    }

    fn render_time(
        &self,
        lua: &mlua::Lua,
        date_time: &DateTime<chrono::Local>,
        message: &Message,
    ) -> Result<Vec<Span>, anyhow::Error> {
        let message = to_lua_message(lua, message)?;
        let date_time = self.date_time_to_table(lua, date_time)?;
        let v = config::emit_sync_callback(lua, ("format-time".to_string(), (date_time, message)))?;

        self.lua_value_to_spans(lua, v)
    }

    fn render_message(
        &self,
        lua: &mlua::Lua,
        message: &Message,
        nickname: &str,
    ) -> Result<Vec<Span>, anyhow::Error> {
        let message = to_lua_message(lua, message)?;
        let v =
            config::emit_sync_callback(lua, ("format-message".to_string(), (message, nickname)))?;

        self.lua_value_to_spans(lua, v)
    }

    fn render_messages(
        &self,
        f: &mut tui::Frame<CrosstermBackend<Stdout>>,
        state: &State,
        lua: &mlua::Lua,
        rect: Rect,
    ) {
        let current_buffer_name = &state.current_buffer;
        let buffers = &state.buffers;

        let current_buffer_messages: &Vec<_> = buffers
            .get(current_buffer_name.to_owned().as_str())
            .unwrap();

        let messages: Vec<_> = current_buffer_messages
            .iter()
            .rev()
            // TODO: Only convert to lua value once
            //.map(|(date_time, message)| {
            //    let message = to_lua_message(lua, message).unwrap();
            //    let date_time = lua.to_value(&date_time).unwrap();
            //    (date_time, message)
            //})
            .map(|(date_time, message)| {
                let time_spans = self
                    .render_time(lua, date_time, message)
                    .unwrap_or_else(|_| vec![]);

                let message_spans = self
                    .render_message(lua, message, &state.nickname)
                    .unwrap_or_else(|_| vec![Span::raw(message.to_string())]);

                if message_spans.is_empty() {
                    return message_spans;
                }

                [time_spans, message_spans].concat()
            })
            .filter(|spans| !spans.is_empty())
            .map(Line::from)
            .map(|message| ListItem::new(message).style(Style::default().fg(Color::White)))
            .collect();

        let list = List::new(messages)
            .block(
                Block::default()
                    .title(format!("{}@{}", state.nickname, state.server))
                    .borders(Borders::NONE),
            )
            .start_corner(tui::layout::Corner::BottomLeft);

        f.render_widget(list, rect);
    }

    fn render_buffer_bar(
        &self,
        f: &mut tui::Frame<CrosstermBackend<Stdout>>,
        state: &State,
        rect: Rect,
    ) {
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

    fn render_input(
        &mut self,
        f: &mut tui::Frame<CrosstermBackend<Stdout>>,
        state: &State,
        input: &Input,
        rect: Rect,
    ) {
        let prefix = match state.mode {
            Mode::Normal => "",
            Mode::Command => ":",
            Mode::Insert => "❯ ",
        };
        let prefix_len = prefix.chars().count() as u16;
        let width = f.size().width.max(3) - prefix_len; // keep 2 for borders and 1 for cursor
        let scroll = input.visual_scroll(width as usize);
        let p = Paragraph::new(format!("{}{}", prefix, input.value()))
            .scroll((0, scroll as u16))
            .block(Block::default().borders(Borders::TOP));
        f.render_widget(p, rect);

        match state.mode {
            Mode::Normal => {}

            Mode::Command | Mode::Insert => {
                // Make the cursor visible and ask tui-rs to put it at the specified coordinates after rendering
                f.set_cursor(
                    // Put cursor past the end of the input text
                    rect.x + ((input.visual_cursor()).max(scroll) - scroll) as u16 + prefix_len,
                    // Move one line down, from the border to the input line
                    rect.y + 1,
                )
            }
        }
    }

    pub fn render(
        &mut self,
        f: &mut tui::Frame<CrosstermBackend<Stdout>>,
        state: &State,
        lua: &mlua::Lua,
        input: &Input,
    ) {
        let layout = self.get_layout();
        let size = f.size();
        let chunks = layout.split(size);

        self.render_messages(f, state, lua, chunks[0]);
        self.render_buffer_bar(f, state, chunks[1]);
        self.render_input(f, state, input, chunks[2]);
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use crate::tui::create_tirc_theme_lua_module;

    #[test]
    fn test_lua_value_to_spans() {
        use super::*;
        let renderer = Renderer::new();
        let lua = mlua::Lua::new();
        let value = lua
            .load(indoc! {"
                { 'Hello', ', ', 'World!' }
            "})
            .eval()
            .unwrap();
        let spans = renderer.lua_value_to_spans(&lua, value).unwrap();
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "Hello");
        assert_eq!(spans[1].content, ", ");
        assert_eq!(spans[2].content, "World!");
    }

    #[test]
    fn test_lua_value_to_spans_nested() {
        use super::*;
        let renderer = Renderer::new();
        let lua = mlua::Lua::new();
        let value = lua
            .load(indoc! {"
                { 'Hello', { ', ' }, 'World!' }
            "})
            .eval()
            .unwrap();
        let spans = renderer.lua_value_to_spans(&lua, value).unwrap();
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "Hello");
        assert_eq!(spans[1].content, ", ");
        assert_eq!(spans[2].content, "World!");
    }

    #[test]
    fn test_lua_value_to_spans_deeply_nested() {
        use super::*;
        let renderer = Renderer::new();
        let lua = mlua::Lua::new();
        let value = lua
            .load(indoc! {"
                { 'a', { 'b', 'c', { 'd', 'e' } }, 'f' }
            "})
            .eval()
            .unwrap();
        let spans = renderer.lua_value_to_spans(&lua, value).unwrap();
        assert_eq!(spans.len(), 6);
        assert_eq!(spans[0].content, "a");
        assert_eq!(spans[1].content, "b");
        assert_eq!(spans[2].content, "c");
        assert_eq!(spans[3].content, "d");
        assert_eq!(spans[4].content, "e");
        assert_eq!(spans[5].content, "f");
    }

    #[test]
    fn test_lua_value_to_styled_spans() -> anyhow::Result<(), anyhow::Error> {
        use super::*;
        let renderer = Renderer::new();
        let lua = mlua::Lua::new();
        create_tirc_theme_lua_module(&lua)?;
        let value = lua
            .load(indoc! {"
                local theme = require('tirc.tui.theme')

                local blue = theme.style { fg = 'blue' }
                local green = theme.style { fg = 'green' }
                local darkgray = theme.style { fg = 'darkgray', bg = 'white' }

                return { { 'a', blue }, { { 'b', { 'c', { 'd', green }, 'e' } }, darkgray }, 'f' }
            "})
            .eval()?;
        let spans = renderer.lua_value_to_spans(&lua, value)?;
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
