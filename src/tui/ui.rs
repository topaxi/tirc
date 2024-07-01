use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use irc::client::Client;
use mlua::Lua;
use std::io::{self, Stdout};
use tui::backend::CrosstermBackend;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use crate::ui::State;

use super::renderer::Renderer;

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

    pub fn render(&mut self, _irc: &Client, lua: &Lua, state: &State) -> Result<(), anyhow::Error> {
        self.terminal.draw(|f| {
            let mut renderer = Renderer::new();

            renderer.render(f, state, lua, &self.input);
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
