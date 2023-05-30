use std::io::{self, Stdout};
use tui;
use tui::backend::CrosstermBackend;

pub struct Terminal {}

impl Terminal {
    pub fn new() -> io::Result<tui::Terminal<CrosstermBackend<Stdout>>> {
        let stdout = io::stdout();
        let backend = CrosstermBackend::new(stdout);
        let terminal = tui::Terminal::new(backend)?;

        Ok(terminal)
    }
}
