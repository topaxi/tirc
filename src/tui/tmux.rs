//! tmux integration for inline images.
//!
//! Inside tmux, graphics escape sequences (iTerm2/Sixel) reach the outer
//! terminal through the passthrough sequence (`ESC Ptmux; .. ESC \`). The outer
//! terminal draws them at its *current cursor position*, which tmux keeps at
//! the cursor of the *active* pane - so an image emitted from an inactive pane
//! lands in the wrong place. The fix is to prefix the image data, inside a
//! single atomic passthrough, with an absolute cursor move to the pane's
//! position in the outer terminal (and save/restore the cursor around it).

use std::process::Command;

/// Whether we are running inside tmux. Mirrors ratatui-image's detection so
/// both sides agree on when escapes need passthrough wrapping.
pub fn in_tmux() -> bool {
    std::env::var("TERM").is_ok_and(|term| term.starts_with("tmux"))
        || std::env::var("TERM_PROGRAM").is_ok_and(|term_program| term_program == "tmux")
}

/// Position of the pane's top-left cell in the outer terminal, 0-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneOrigin {
    pub row: u16,
    pub col: u16,
}

/// Asks tmux where our pane sits in the outer terminal. Returns `None` (with a
/// warning) when tmux cannot be queried, in which case the renderer falls back
/// to suppressing images while unfocused.
///
/// The query must target our own pane explicitly (`$TMUX_PANE`): without a
/// target, `display -p` reports the client's *active* pane, which is somebody
/// else's whenever we are the unfocused pane - exactly the case this position
/// is needed for.
pub fn query_pane_origin() -> Option<PaneOrigin> {
    let mut command = Command::new("tmux");
    command.arg("display");
    match std::env::var("TMUX_PANE") {
        Ok(pane) if !pane.is_empty() => {
            command.args(["-t", &pane]);
        }
        _ => log::warn!("TMUX_PANE is not set; tmux pane origin may track the wrong pane"),
    }
    let output = command
        .args([
            "-p",
            "#{pane_left}\t#{pane_top}\t#{status}\t#{status-position}",
        ])
        .output();

    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let origin = parse_pane_origin(stdout.trim());
            if origin.is_none() {
                log::warn!("could not parse tmux pane origin from {stdout:?}");
            }
            origin
        }
        Ok(output) => {
            log::warn!(
                "tmux pane origin query failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            None
        }
        Err(err) => {
            log::warn!("could not run tmux to query pane origin: {err}");
            None
        }
    }
}

/// Parses `pane_left \t pane_top \t status \t status-position`. The pane
/// coordinates are relative to the window area, which sits below the status
/// line when it is positioned at the top.
fn parse_pane_origin(s: &str) -> Option<PaneOrigin> {
    let mut fields = s.split('\t');
    let col: u16 = fields.next()?.trim().parse().ok()?;
    let row: u16 = fields.next()?.trim().parse().ok()?;
    let status = fields.next()?.trim();
    let status_position = fields.next()?.trim();

    let status_lines: u16 = match status {
        "off" => 0,
        "on" => 1,
        n => n.parse().ok()?,
    };
    let row = if status_position == "top" {
        row.checked_add(status_lines)?
    } else {
        row
    };

    Some(PaneOrigin { row, col })
}

/// Wraps already escape-doubled graphics data in a plain tmux passthrough,
/// without positioning: the data draws at the outer terminal's current cursor.
/// Only correct while this pane is active; used as the fallback when the pane
/// origin could not be queried.
pub fn wrap_passthrough(data_doubled: &str) -> String {
    format!("\x1bPtmux;{data_doubled}\x1b\\")
}

/// Wraps already escape-doubled graphics data in a single tmux passthrough
/// that saves the outer cursor, moves it to the absolute cell (0-based), lets
/// the data draw there, and restores the cursor. One passthrough per image so
/// tmux forwards it atomically and its own output cannot interleave between
/// the positioning and the image data.
pub fn wrap_passthrough_positioned(data_doubled: &str, row: u16, col: u16) -> String {
    let mut wrapped = String::with_capacity(data_doubled.len() + 32);
    wrapped.push_str("\x1bPtmux;\x1b\x1b[s");
    // CUP is 1-based.
    wrapped.push_str(&format!(
        "\x1b\x1b[{};{}H",
        u32::from(row) + 1,
        u32::from(col) + 1
    ));
    wrapped.push_str(data_doubled);
    wrapped.push_str("\x1b\x1b[u\x1b\\");
    wrapped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pane_origin_status_on_top() {
        assert_eq!(
            parse_pane_origin("117\t0\ton\ttop"),
            Some(PaneOrigin { row: 1, col: 117 })
        );
    }

    #[test]
    fn test_parse_pane_origin_status_on_bottom() {
        assert_eq!(
            parse_pane_origin("0\t23\ton\tbottom"),
            Some(PaneOrigin { row: 23, col: 0 })
        );
    }

    #[test]
    fn test_parse_pane_origin_status_off() {
        assert_eq!(
            parse_pane_origin("5\t7\toff\ttop"),
            Some(PaneOrigin { row: 7, col: 5 })
        );
    }

    #[test]
    fn test_parse_pane_origin_multiline_status() {
        assert_eq!(
            parse_pane_origin("0\t2\t3\ttop"),
            Some(PaneOrigin { row: 5, col: 0 })
        );
    }

    #[test]
    fn test_parse_pane_origin_garbage() {
        assert_eq!(parse_pane_origin(""), None);
        assert_eq!(parse_pane_origin("a\tb\ton\ttop"), None);
        assert_eq!(parse_pane_origin("1\t2"), None);
    }

    #[test]
    fn test_wrap_passthrough_positioned() {
        let wrapped = wrap_passthrough_positioned("\x1b\x1b]1337;File=x\x07", 1, 117);
        assert_eq!(
            wrapped,
            "\x1bPtmux;\x1b\x1b[s\x1b\x1b[2;118H\x1b\x1b]1337;File=x\x07\x1b\x1b[u\x1b\\"
        );
    }

    #[test]
    fn test_wrap_passthrough_positioned_origin_is_one_based() {
        let wrapped = wrap_passthrough_positioned("data", 0, 0);
        assert!(wrapped.contains("\x1b\x1b[1;1H"));
        assert!(wrapped.starts_with("\x1bPtmux;"));
        assert!(wrapped.ends_with("\x1b\\"));
    }
}
