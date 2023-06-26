use std::io::Stdout;

use irc::proto::Message;
use mlua::{FromLua, LuaSerdeExt};
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

pub struct Renderer {}

fn to_lua_message<'lua>(
    lua: &'lua mlua::Lua,
    message: &Message,
) -> mlua::Result<mlua::Table<'lua>> {
    let lua_message = lua.to_value(message)?;

    match lua_message {
        mlua::Value::Table(table) => {
            let metatable = lua.create_table().expect("Unable to create metatable");
            let lua_message_str = lua.to_value(&message.to_string())?;

            metatable.set("__str", lua_message_str)?;
            metatable
                .set(
                    "__tostring",
                    lua.create_function(|_, lua_message: mlua::Value<'_>| {
                        Ok(match lua_message {
                            mlua::Value::Table(tbl) => tbl.get_metatable().unwrap().get("__str"),
                            _ => Ok(None::<String>),
                        })
                    })
                    .expect("Unable to create __tostring function"),
                )
                .expect("Unable to set __tostring function on metatable");

            table.set_metatable(Some(metatable));

            Ok(table)
        }
        _ => Err(mlua::Error::external(anyhow::anyhow!(
            "message must be a table"
        ))),
    }
}

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

    fn table_to_spans(&self, lua: &mlua::Lua, table: mlua::Table) -> mlua::Result<Vec<Span>> {
        let mut spans = vec![];

        for v in table.sequence_values::<mlua::Value>() {
            let v = v?;

            match v {
                mlua::Value::String(_) => {
                    let v = mlua::String::from_lua(v, lua)?;
                    let str = v.to_str()?.to_owned();

                    spans.push(Span::from(str));
                }
                mlua::Value::Table(v) => {
                    let str = v.get::<_, Option<String>>(1)?;
                    let style = v.get::<_, Option<mlua::Table>>(2)?;

                    if let Some(str) = str {
                        if let Some(style) = style {
                            let style: Style = lua.from_value(mlua::Value::Table(style))?;

                            spans.push(Span::styled(str, style));
                        } else {
                            spans.push(Span::from(str));
                        }
                    }
                }
                _ => {
                    return Err(mlua::Error::external(anyhow::anyhow!(
                        "table must be a string or a table with a string and style"
                    )))
                }
            }
        }

        Ok(spans)
    }

    fn render_time(&self, lua: &mlua::Lua, message: &Message) -> Result<Vec<Span>, anyhow::Error> {
        let message = to_lua_message(lua, message)?;
        let v = config::emit_sync_callback(lua, ("format-time".to_string(), (message)))?;

        match &v {
            mlua::Value::String(_) => {
                let v = mlua::String::from_lua(v, lua)?;
                let str = v.to_str()?.to_owned();
                Ok(vec![Span::from(str)])
            }
            mlua::Value::Table(tbl) => Ok(self.table_to_spans(lua, tbl.to_owned())?),
            _ => Err(anyhow::anyhow!("format-time callback must return a string")),
        }
    }

    fn render_message(
        &self,
        lua: &mlua::Lua,
        message: &Message,
    ) -> Result<Vec<Span>, anyhow::Error> {
        let message = to_lua_message(lua, message)?;
        let v = config::emit_sync_callback(lua, ("format-message".to_string(), (message)))?;

        match &v {
            mlua::Value::Nil => Ok(vec![]),
            mlua::Value::String(_) => {
                let v = mlua::String::from_lua(v, lua)?;
                let str = v.to_str()?.to_owned();

                Ok(vec![Span::raw(str)])
            }
            mlua::Value::Table(tbl) => Ok(self.table_to_spans(lua, tbl.to_owned())?),
            _ => Err(anyhow::anyhow!(
                "render-message callback must return a string or nil"
            )),
        }
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

        let current_buffer_messages: &Vec<Message> = buffers
            .get(current_buffer_name.to_owned().as_str())
            .unwrap();

        let messages: Vec<_> = current_buffer_messages
            .iter()
            .rev()
            .map(|message| {
                let time_spans = self.render_time(lua, message).unwrap_or_else(|_| vec![]);

                let message_spans = self
                    .render_message(lua, message)
                    .unwrap_or_else(|_| vec![Span::raw(message.to_string())]);

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
