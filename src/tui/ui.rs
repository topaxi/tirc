use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{self, Stdout};
use tui;
use tui::backend::CrosstermBackend;
use tui::layout::{Constraint, Direction, Layout};
use tui::style::{Color, Style};
use tui::widgets::{Block, Borders, List, ListItem, Paragraph};
use tui_input::Input;

use crate::ui::Mode;
use crate::ui::State;

pub struct Ui {
    terminal: tui::Terminal<CrosstermBackend<Stdout>>,
    pub input: Input,
}

impl Ui {
    pub fn new() -> io::Result<Ui> {
        let stdout = io::stdout();
        let backend = CrosstermBackend::new(stdout);
        let terminal = tui::Terminal::new(backend)?;

        Ok(Ui {
            terminal,
            input: Input::default(),
        })
    }

    pub fn initialize_terminal(&mut self) -> Result<(), failure::Error> {
        enable_raw_mode()?;

        self.terminal.clear()?;

        Ok(())
    }

    fn get_layout(&self) -> Layout {
        return Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(2)].as_ref());
    }

    pub fn render(&mut self, state: &State) -> Result<(), failure::Error> {
        let layout = self.get_layout();

        self.terminal.draw(|f| {
            let size = f.size();
            let chunks = layout.split(size);

            let messages: Vec<_> = state
                .messages
                .iter()
                .rev()
                .take(chunks[0].height as usize)
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

            f.render_widget(list, chunks[0]);
            f.render_widget(input, chunks[1]);

            match state.mode {
                Mode::Normal => {}

                Mode::Command | Mode::Insert => {
                    // Make the cursor visible and ask tui-rs to put it at the specified coordinates after rendering
                    f.set_cursor(
                        // Put cursor past the end of the input text
                        chunks[1].x
                            + ((self.input.visual_cursor()).max(scroll) - scroll) as u16
                            + prefix_len as u16,
                        // Move one line down, from the border to the input line
                        chunks[1].y + 1,
                    )
                }
            }
        })?;

        Ok(())
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        disable_raw_mode().unwrap();
    }
}
