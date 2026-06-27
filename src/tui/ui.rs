use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use mlua::Lua;
use ratatui::backend::CrosstermBackend;
use std::io::{self, Stdout};
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use crate::ui::{State, ViewState};

use super::renderer::Renderer;

pub struct Tui {
    terminal: ratatui::Terminal<CrosstermBackend<Stdout>>,
    input: Input,
    renderer: Renderer,
}

impl Tui {
    pub fn new() -> io::Result<Self> {
        let stdout = io::stdout();
        let backend = CrosstermBackend::new(stdout);
        let terminal = ratatui::Terminal::new(backend)?;

        Ok(Self {
            terminal,
            input: Input::default(),
            renderer: Renderer::default(),
        })
    }

    pub fn install_panic_hook() {
        let original = std::panic::take_hook();

        std::panic::set_hook(Box::new(move |info| {
            let _ = Tui::restore_terminal();
            original(info);
        }));
    }

    fn restore_terminal() -> io::Result<()> {
        disable_raw_mode()?;
        execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;

        Ok(())
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

    pub fn render(
        &mut self,
        lua: &Lua,
        state: &State,
        view: &ViewState,
    ) -> Result<(), anyhow::Error> {
        self.terminal.draw(|f| {
            self.renderer.render(f, state, view, lua, &self.input);
        })?;

        Ok(())
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = disable_raw_mode();

        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
    }
}
