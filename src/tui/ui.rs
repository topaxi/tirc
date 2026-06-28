use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, Clear, ClearType,
    EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, queue};
use mlua::Lua;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use std::io::{self, Stdout};
use std::ops::RangeInclusive;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use crate::ui::{State, ViewState};

use super::renderer::Renderer;

pub struct Tui {
    terminal: ratatui::Terminal<CrosstermBackend<Stdout>>,
    input: Input,
    renderer: Renderer,
    /// A clone of the most recently rendered frame's cell buffer. The terminal's
    /// own back/front buffers are swapped and reset by `draw`, so the rendered
    /// cells are not readable afterwards; keeping a copy here lets the yank
    /// command read the exact text the user saw and selected. `None` until the
    /// first frame is drawn.
    last_frame: Option<Buffer>,
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
            last_frame: None,
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

    pub fn set_input(&mut self, value: &str) {
        self.input = value.into();
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
        queue!(self.terminal.backend_mut(), Clear(ClearType::All))?;
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
        // buggy incremental path entirely, but a full erase+repaint flickers, so
        // wrap the frame in a synchronized update (terminal mode 2026): the
        // terminal buffers the erase and the repaint and swaps to them atomically.
        // Terminals without support ignore these escapes and just fall back to the
        // (flickering) erase+repaint. Remove once the upstream bug is fixed.
        queue!(self.terminal.backend_mut(), BeginSynchronizedUpdate)?;

        self.redraw()?;

        // Clone the freshly rendered cell buffer before ending the synchronized
        // update: `draw` swaps and resets the terminal's internal buffers, so
        // this is the only point the rendered cells are readable. The clone ends
        // the immutable terminal borrow before `backend_mut` below.
        let frame = self
            .terminal
            .draw(|f| {
                self.renderer.render(f, state, view, lua, &self.input);
            })?
            .buffer
            .clone();
        self.last_frame = Some(frame);

        execute!(self.terminal.backend_mut(), EndSynchronizedUpdate)?;

        Ok(())
    }

    /// Releases terminal mouse capture so the terminal performs its own native
    /// text selection (the release-capture "copy mode"). While capture is off the
    /// app receives no mouse events.
    pub fn disable_mouse_capture(&mut self) -> Result<(), anyhow::Error> {
        execute!(self.terminal.backend_mut(), DisableMouseCapture)?;
        Ok(())
    }

    /// Re-enables terminal mouse capture, restoring app-level scroll/click/drag
    /// handling when copy mode is left.
    pub fn enable_mouse_capture(&mut self) -> Result<(), anyhow::Error> {
        execute!(self.terminal.backend_mut(), EnableMouseCapture)?;
        Ok(())
    }

    /// Reads the text of the most recently rendered frame over `rows`, taking the
    /// full `x0..=x1` column span of each row (the message area's width for
    /// line-granular selection). Returns an empty string before the first frame
    /// is drawn. See [`buffer_text`] for the row-assembly rules.
    pub fn selection_text(&self, rows: RangeInclusive<u16>, x0: u16, x1: u16) -> String {
        match &self.last_frame {
            Some(buffer) => buffer_text(buffer, rows, x0, x1),
            None => String::new(),
        }
    }
}

/// Assembles the visible text of `buffer` over `rows` and the inclusive column
/// span `x0..=x1`. Each row is the concatenation of its cell symbols (wide
/// graphemes already occupy their first cell with empty trailing cells, so this
/// does not double-count), with trailing whitespace trimmed per row. Trailing
/// blank rows are dropped, and rows are joined with `\n`. Coordinates outside the
/// buffer's area are skipped rather than panicking, so a selection captured from
/// an earlier frame survives a resize.
fn buffer_text(buffer: &Buffer, rows: RangeInclusive<u16>, x0: u16, x1: u16) -> String {
    let mut lines: Vec<String> = Vec::new();

    for y in rows {
        let mut line = String::new();
        for x in x0..=x1 {
            if let Some(cell) = buffer.cell((x, y)) {
                line.push_str(cell.symbol());
            }
        }
        // Trim trailing whitespace so the padding the renderer writes to the end
        // of each row does not bloat the copied text.
        lines.push(line.trim_end().to_string());
    }

    // Drop trailing empty rows (e.g. a selection that ran past the last message).
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }

    lines.join("\n")
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

#[cfg(test)]
mod tests {
    use super::buffer_text;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn buffer_with(lines: &[&str]) -> Buffer {
        Buffer::with_lines(lines.iter().copied())
    }

    #[test]
    fn buffer_text_trims_trailing_whitespace_per_row() {
        // Each line is padded to width 8; the selection takes the full width.
        let buffer = buffer_with(&["hello   ", "hi      "]);
        let text = buffer_text(&buffer, 0..=1, 0, 7);
        assert_eq!(text, "hello\nhi");
    }

    #[test]
    fn buffer_text_drops_trailing_empty_rows() {
        let buffer = buffer_with(&["line one", "        ", "        "]);
        // A selection running past the last text line must not keep blank rows.
        let text = buffer_text(&buffer, 0..=2, 0, 7);
        assert_eq!(text, "line one");
    }

    #[test]
    fn buffer_text_keeps_interior_blank_rows() {
        let buffer = buffer_with(&["a       ", "        ", "b       "]);
        // A blank row between two text rows is preserved.
        let text = buffer_text(&buffer, 0..=2, 0, 7);
        assert_eq!(text, "a\n\nb");
    }

    #[test]
    fn buffer_text_honours_the_column_span() {
        let buffer = buffer_with(&["abcdefgh"]);
        // Only columns 2..=4 are selected.
        let text = buffer_text(&buffer, 0..=0, 2, 4);
        assert_eq!(text, "cde");
    }

    #[test]
    fn buffer_text_skips_out_of_bounds_cells() {
        // A 4-wide buffer with a selection captured from a wider earlier frame:
        // out-of-range columns and rows are skipped rather than panicking.
        let mut buffer = buffer_with(&["word"]);
        buffer.resize(Rect::new(0, 0, 4, 1));
        let text = buffer_text(&buffer, 0..=5, 0, 20);
        assert_eq!(text, "word");
    }
}
