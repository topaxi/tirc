use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use irc::client::Client;
use irc::proto::Message;
use mlua::Lua;
use std::io::{self, Stdout};
use tui;
use tui::backend::CrosstermBackend;
use tui::layout::{Constraint, Direction, Layout};
use tui::style::{Color, Modifier, Style};
use tui::text::{Line, Span};
use tui::widgets::{Block, Borders, List, ListItem, Paragraph};
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use crate::ui::Mode;
use crate::ui::State;

pub struct Tui {
    terminal: tui::Terminal<CrosstermBackend<Stdout>>,
    input: Input,
}

impl Tui {
    pub fn new() -> io::Result<Tui> {
        let stdout = io::stdout();
        let backend = CrosstermBackend::new(stdout);
        let terminal = tui::Terminal::new(backend)?;

        Ok(Tui {
            terminal,
            input: Input::default(),
        })
    }

    pub fn input(&self) -> &Input {
        &self.input
    }

    pub fn reset_input(&mut self) {
        self.input.reset();
    }

    pub fn handle_event(&mut self, event: &crossterm::event::Event) {
        self.input.handle_event(event);
    }

    pub fn initialize_terminal(&mut self) -> Result<(), anyhow::Error> {
        enable_raw_mode()?;

        self.terminal.clear()?;

        execute!(
            self.terminal.backend_mut(),
            EnterAlternateScreen,
            EnableMouseCapture
        )?;

        Ok(())
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

    pub fn render(
        &mut self,
        _irc: &Client,
        _lua: &Lua,
        state: &State,
    ) -> Result<(), anyhow::Error> {
        let layout = self.get_layout();

        self.terminal.draw(|f| {
            let size = f.size();
            let chunks = layout.split(size);
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

            let prefix = match state.mode {
                Mode::Normal => "",
                Mode::Command => ":",
                Mode::Insert => "❯ ",
            };

            let prefix_len = prefix.chars().count() as u16;
            let width = chunks[1].width.max(3) - prefix_len; // keep 2 for borders and 1 for cursor
            let scroll = self.input.visual_scroll(width as usize);

            let input = Paragraph::new(format!("{}{}", prefix, self.input.value()))
                .scroll((0, scroll as u16))
                .block(Block::default().borders(Borders::TOP));

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

            f.render_widget(list, chunks[0]);
            f.render_widget(buffer_bar, chunks[1]);
            f.render_widget(input, chunks[2]);

            match state.mode {
                Mode::Normal => {}

                Mode::Command | Mode::Insert => {
                    // Make the cursor visible and ask tui-rs to put it at the specified coordinates after rendering
                    f.set_cursor(
                        // Put cursor past the end of the input text
                        chunks[2].x
                            + ((self.input.visual_cursor()).max(scroll) - scroll) as u16
                            + prefix_len,
                        // Move one line down, from the border to the input line
                        chunks[2].y + 1,
                    )
                }
            }
        })?;

        Ok(())
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        disable_raw_mode().unwrap();

        execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )
        .unwrap();
    }
}
