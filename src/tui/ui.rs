use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
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
        execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            DisableBracketedPaste
        )?;

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
            EnableMouseCapture,
            EnableBracketedPaste
        )?;

        Ok(())
    }

    /// Queues a full repaint that takes effect on the next [`Self::render`]:
    /// resets the back buffer so the whole frame is re-emitted, and queues a
    /// screen erase so cells vacated since the last frame are cleared.
    ///
    /// The erase is *queued*, not flushed, so it goes out together with the next
    /// `draw` (one flush, no flicker). It deliberately avoids `Terminal::clear`,
    /// which issues a cursor-position query (DSR) and reads the reply from stdin -
    /// that races the async input reader and fails with "cursor position could not
    /// be read".
    ///
    /// Exposed as the manual `:redraw` command and Ctrl-L. These are now largely
    /// redundant since [`Self::render`] repaints fully every frame, but kept as an
    /// explicit escape hatch.
    pub fn redraw(&mut self) -> Result<(), anyhow::Error> {
        crossterm::queue!(
            self.terminal.backend_mut(),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
        )?;
        self.terminal.swap_buffers();
        Ok(())
    }

    pub fn render(
        &mut self,
        lua: &Lua,
        state: &State,
        view: &mut ViewState,
    ) -> Result<(), anyhow::Error> {
        // Workaround for https://github.com/ratatui/ratatui/issues/2357: ratatui's
        // incremental buffer diff mis-renders lines containing wide graphemes
        // (notably emoji-presentation sequences with U+FE0F), leaving stale cells
        // and spurious spacing. Forcing a full repaint every frame sidesteps the
        // buggy incremental path entirely. The queued erase + back-buffer reset are
        // flushed together with the `draw` below, so this neither flickers nor
        // queries the cursor. Remove once the upstream bug is fixed.
        self.redraw()?;

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
            DisableMouseCapture,
            DisableBracketedPaste
        );
    }
}
