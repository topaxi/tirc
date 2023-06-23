use std::io::Stdout;

use irc::proto::Message;
use tui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
};
use tui_input::Input;

use crate::ui::{Mode, State};

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

    fn render_messages(
        &self,
        f: &mut tui::Frame<CrosstermBackend<Stdout>>,
        state: &State,
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
                ListItem::new(message.to_string()).style(Style::default().fg(Color::White))
            })
            .rev()
            .collect();

        let list = List::new(messages).block(
            Block::default()
                .title("irc.topaxi.ch")
                .borders(Borders::NONE),
        );

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
        _lua: &mlua::Lua,
        input: &Input,
    ) {
        let layout = self.get_layout();
        let size = f.size();
        let chunks = layout.split(size);

        self.render_messages(f, state, chunks[0]);
        self.render_buffer_bar(f, state, chunks[1]);
        self.render_input(f, state, input, chunks[2]);
    }
}
