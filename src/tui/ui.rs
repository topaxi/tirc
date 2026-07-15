use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture,
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

use ratatui_image::picker::{Picker, ProtocolType};

use crate::config::ImageProtocol;
use crate::ui::{State, ViewState};

use super::renderer::Renderer;
use super::{DecodeRequest, DecodedImage, PreviewRequest, PreviewResult};
use tokio::sync::mpsc::UnboundedSender;

/// Maps the configured [`ImageProtocol`] to a forced [`ProtocolType`], or `None`
/// for auto-detection.
fn forced_protocol(image_protocol: ImageProtocol) -> Option<ProtocolType> {
    match image_protocol {
        ImageProtocol::Auto => None,
        ImageProtocol::Kitty => Some(ProtocolType::Kitty),
        ImageProtocol::Sixel => Some(ProtocolType::Sixel),
        ImageProtocol::Iterm2 => Some(ProtocolType::Iterm2),
    }
}

/// Builds the image [`Picker`] honoring the configured protocol. Auto-detection
/// queries the terminal for both protocol and font size. A forced protocol still
/// queries for the font size, but overrides the protocol; if the query fails
/// entirely, it falls back to an assumed font size so forcing still works on
/// terminals that do not answer.
fn build_picker(image_protocol: ImageProtocol) -> Option<Picker> {
    let forced = forced_protocol(image_protocol);
    let (mut picker, source) = match Picker::from_query_stdio() {
        Ok(picker) => (
            picker,
            if forced.is_some() {
                "forced"
            } else {
                "auto-detected"
            },
        ),
        Err(err) => match forced {
            // `halfblocks()` assumes a font size (and detects tmux) without a
            // query, so a forced protocol still renders on terminals that do not
            // answer the query, only at an approximate scale.
            Some(_) => (Picker::halfblocks(), "forced (font size assumed)"),
            None => {
                log::warn!("terminal image support unavailable: {err}");
                return None;
            }
        },
    };

    if let Some(protocol) = forced {
        picker.set_protocol_type(protocol);
    }

    log::info!(
        "inline image protocol: {:?} ({source}), font {:?}",
        picker.protocol_type(),
        picker.font_size()
    );

    Some(picker)
}

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
            DisableBracketedPaste,
            DisableFocusChange
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

    /// Records terminal focus so inline images are only emitted while our pane is
    /// active (see [`Renderer::set_focused`](super::renderer::Renderer::set_focused)).
    pub fn set_focused(&mut self, focused: bool) {
        self.renderer.set_focused(focused);
    }

    /// Configures the quick-reaction affordance from the user config, forwarded to
    /// the renderer which draws the selected-message pill bar.
    pub fn set_quick_reactions(&mut self, config: &crate::config::QuickReactions) {
        self.renderer.set_quick_reactions(config);
    }

    /// Prepares the terminal and, if graphics are supported, returns the built
    /// [`Picker`] so the caller can spawn the background image-decode worker with
    /// it. `None` means inline images are disabled and media falls back to text.
    pub fn initialize_terminal(
        &mut self,
        image_protocol: crate::config::ImageProtocol,
    ) -> Result<Option<Picker>, anyhow::Error> {
        enable_raw_mode()?;

        // Set up terminal graphics for inline images. Must run before the async
        // stdin reader starts (it reads the query reply) and while raw mode is on.
        // Best-effort: on failure, inline images are disabled and media falls back
        // to its textual line.
        let picker = build_picker(image_protocol);
        if picker.is_some() {
            self.renderer.enable_images();
        }

        self.terminal.clear()?;

        execute!(
            self.terminal.backend_mut(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            EnableFocusChange
        )?;

        Ok(picker)
    }

    /// Wires the channel the renderer uses to request background image decodes.
    pub fn set_decode_sender(&mut self, tx: UnboundedSender<DecodeRequest>) {
        self.renderer.set_decode_sender(tx);
    }

    /// Feeds a finished background decode into the renderer's image cache.
    pub fn insert_decoded_image(&mut self, decoded: DecodedImage) {
        self.renderer.insert_decoded(decoded);
    }

    /// Wires the channel the renderer uses to request background link previews.
    pub fn set_preview_sender(&mut self, tx: UnboundedSender<PreviewRequest>) {
        self.renderer.set_preview_sender(tx);
    }

    /// Feeds a finished link-preview fetch into the renderer's preview cache.
    pub fn insert_link_preview(&mut self, result: PreviewResult) {
        self.renderer.insert_link_preview(result);
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
            DisableBracketedPaste,
            DisableFocusChange
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
