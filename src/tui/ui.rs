use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use irc::proto::Message;
use std::io::{self, Stdout};
use tui;
use tui::backend::CrosstermBackend;
use tui::style::{Color, Style};
use tui::widgets::{Block, Borders, List, ListItem};

pub struct Ui {
    terminal: tui::Terminal<CrosstermBackend<Stdout>>,
}

impl Ui {
    pub fn new() -> io::Result<Ui> {
        let stdout = io::stdout();
        let backend = CrosstermBackend::new(stdout);
        let terminal = tui::Terminal::new(backend)?;

        Ok(Ui { terminal })
    }

    pub fn initialize_terminal(&mut self) -> Result<(), failure::Error> {
        enable_raw_mode()?;

        self.terminal.clear()?;

        Ok(())
    }

    pub fn render(&mut self, messages: &Vec<Message>) -> Result<(), failure::Error> {
        self.terminal.draw(|f| {
            let size = f.size();
            let messages: Vec<_> = messages
                .iter()
                .rev()
                .take(size.height as usize)
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

            f.render_widget(list, size);
        })?;

        Ok(())
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        disable_raw_mode().unwrap();
    }
}
