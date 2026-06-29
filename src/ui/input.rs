use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crossterm::event::{
    Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind,
};
use mlua::Lua;

use crate::backends::BackendHandle;
use crate::config::{
    collect_user_watched_paths, emit_event, reload_lua_theme, EventName, SelectionMode,
};
use crate::core::{
    BackendEvent, BackendId, BackendMessage, ChatEvent, Command, MsgKind, TargetId, TxnAllocator,
    VerifyAction,
};
use crate::tui::lua::{create_lua_sender, to_lua_event};
use crate::tui::Tui;

use super::state::StoredMessage;
use super::{MenuAction, MenuItem, MenuTarget, Mode, Selection, State, ViewState};

/// Events the main loop feeds to the input handler.
#[derive(Debug)]
pub enum Event {
    Input(KeyEvent),
    Mouse(crossterm::event::MouseEvent),
    Paste(String),
    Backend(BackendMessage),
    Tick,
}

pub struct InputHandler<'lua> {
    lua: &'lua Lua,
    ui: Tui,
    backends: Vec<BackendHandle>,
    txn: Arc<TxnAllocator>,
    /// Lazily-built per-backend Lua sender tables passed to `event` handlers,
    /// cached so we do not rebuild closures on every event.
    senders: HashMap<BackendId, mlua::RegistryKey>,
    config_path: PathBuf,
    auto_reload: bool,
    /// Extra watch paths from `config.watch_files`, relative to the config dir.
    extra_watch_files: Vec<String>,
    /// Files being polled for mtime changes; rebuilt after each reload.
    watched_files: Vec<(PathBuf, SystemTime)>,
    history: History,
    /// Set when something that affects the rendered frame changed; the main
    /// loop renders only when this is set, so idle ticks and mouse moves do not
    /// trigger a repaint.
    dirty: bool,
    /// True while a left-button drag started on the sidebar split boundary, so
    /// subsequent `Drag` events resize the sidebar rather than being ignored.
    dragging_split: bool,
    /// True while a left-button drag is extending a message-area text selection,
    /// so subsequent `Drag` events update the selection cursor.
    selecting: bool,
    /// The configured default mouse-drag behaviour. In [`SelectionMode::Native`]
    /// a drag does not select in-app; the user relies on the always-available
    /// copy-mode toggle instead.
    selection_mode: SelectionMode,
}

impl<'lua> InputHandler<'lua> {
    // The handler genuinely owns this many collaborators; grouping them into a
    // struct would only move the argument list to a builder for no clarity gain.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        lua: &'lua Lua,
        ui: Tui,
        backends: Vec<BackendHandle>,
        txn: Arc<TxnAllocator>,
        config_path: PathBuf,
        auto_reload: bool,
        extra_watch_files: Vec<String>,
        selection_mode: SelectionMode,
    ) -> Self {
        let watched_files = if auto_reload {
            Self::build_watch_list_for(lua, &config_path, &extra_watch_files)
        } else {
            vec![]
        };

        Self {
            lua,
            ui,
            backends,
            txn,
            senders: HashMap::new(),
            config_path,
            auto_reload,
            extra_watch_files,
            watched_files,
            history: History::default(),
            dirty: true,
            dragging_split: false,
            selecting: false,
            selection_mode,
        }
    }

    /// Marks the frame as needing a repaint on the next loop iteration.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Returns whether a repaint is needed and clears the flag.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.dirty, false)
    }

    fn build_watch_list_for(
        lua: &Lua,
        config_path: &Path,
        extra_watch_files: &[String],
    ) -> Vec<(PathBuf, SystemTime)> {
        let config_dir = config_path.parent().unwrap_or(config_path);
        #[allow(unused_mut)]
        let mut paths = collect_user_watched_paths(lua, config_dir, config_path, extra_watch_files);

        #[cfg(all(debug_assertions, not(test)))]
        paths.extend(crate::config::builtin_lua_paths());

        paths
            .into_iter()
            .filter_map(|p| {
                let mtime = std::fs::metadata(&p).ok()?.modified().ok()?;
                Some((p, mtime))
            })
            .collect()
    }

    fn refresh_watched_files(&mut self) {
        if self.auto_reload {
            self.watched_files =
                Self::build_watch_list_for(self.lua, &self.config_path, &self.extra_watch_files);
        }
    }

    /// Reloads the Lua theme/config, reports the result to the status buffer of
    /// `backend`, and refreshes the file watch list on success.
    fn do_reload(&mut self, state: &mut State, backend: Option<BackendId>) {
        let notice_text = match reload_lua_theme(self.lua, &self.config_path) {
            Ok(()) => {
                self.refresh_watched_files();
                "Theme reloaded successfully".to_owned()
            }
            Err(err) => format!("Reload error: {err}").replace(['\r', '\n'], " "),
        };

        if let Some(backend) = backend {
            state.apply(
                backend,
                ChatEvent::ServerInfo {
                    target: None,
                    from: None,
                    code: None,
                    text: notice_text,
                    raw: None,
                },
            );
        }
    }

    /// Returns whether the tick reloaded the config/theme (and thus changed the
    /// frame). The file polling itself runs every tick regardless.
    fn handle_tick(&mut self, state: &mut State, view: &ViewState) -> bool {
        if !self.auto_reload || self.watched_files.is_empty() {
            return false;
        }

        let mut changed = false;
        for (path, mtime) in &mut self.watched_files {
            if let Some(new_mtime) = std::fs::metadata(&*path)
                .ok()
                .and_then(|m| m.modified().ok())
            {
                if new_mtime != *mtime {
                    *mtime = new_mtime;
                    changed = true;
                }
            }
        }

        if changed {
            let backend = view.focused.as_ref().map(|b| b.backend);
            self.do_reload(state, backend);
        }

        changed
    }

    pub fn ui(&self) -> &Tui {
        &self.ui
    }

    pub fn render_ui(&mut self, state: &State, view: &mut ViewState) -> Result<(), anyhow::Error> {
        self.ui.render(self.lua, state, view)
    }

    fn backend(&self, id: BackendId) -> Option<&BackendHandle> {
        self.backends.iter().find(|b| b.id() == id)
    }

    /// Enqueues an outgoing message; the backend echoes it back as an optimistic
    /// local copy, so we do not touch state here.
    fn send(&self, id: BackendId, target: TargetId, body: String, kind: MsgKind) {
        if let Some(backend) = self.backend(id) {
            let _ = backend.send(Command::SendMessage {
                target,
                body,
                kind,
                txn: self.txn.next(),
            });
        }
    }

    /// Returns whether the event scrolled the buffer (and thus changed the frame).
    fn handle_mouse(
        &mut self,
        state: &mut State,
        view: &mut ViewState,
        event: crossterm::event::MouseEvent,
    ) -> bool {
        // While the context menu is open it is modal: every mouse path is handled
        // by the menu and must not fall through to scroll/drag/click. Each menu
        // path returns whether it changed the frame so `handle_event` repaints.
        if view.menu.open {
            return self.handle_menu_mouse(state, view, event);
        }

        let delta = 3usize;
        match event.kind {
            MouseEventKind::ScrollUp => {
                if let Some(buffer) = state.focused_buffer_mut(view) {
                    buffer.scroll_up(delta);
                }
                true
            }
            MouseEventKind::ScrollDown => {
                if let Some(buffer) = state.focused_buffer_mut(view) {
                    buffer.scroll_down(delta);
                }
                true
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // A fresh press drops any prior selection; it is re-established
                // below when the press starts a new one. Clearing here also
                // repaints away a stale highlight when the click does something
                // else (a tab switch, a split-drag, an empty click).
                let had_selection = view.selection.take().is_some();
                // A press on the split boundary begins a resize drag; a press in
                // the message area begins a text selection; otherwise fall
                // through to tab/user-row handling. `had_selection` keeps the
                // frame repainting when only the cleared highlight changed.
                self.try_start_split_drag(view, event.column, event.row)
                    || self.try_start_selection(view, event.column, event.row)
                    || self.handle_left_click(state, view, event.column, event.row)
                    || had_selection
            }
            MouseEventKind::Down(MouseButton::Right) => {
                self.handle_right_click(state, view, event.column, event.row)
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging_split => {
                self.drag_split(view, event.column)
            }
            MouseEventKind::Drag(MouseButton::Left) if self.selecting => {
                self.update_selection(view, event.column, event.row)
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.dragging_split = false;
                // Stop extending the selection but keep it visible so the user
                // can yank it. Repaint only if a drag was actually in progress.
                let was_selecting = self.selecting;
                self.selecting = false;
                was_selecting
            }
            _ => false,
        }
    }

    /// Begins a message-area text selection when a left-press lands inside the
    /// message rect and app selection is enabled (not [`SelectionMode::Native`]
    /// and not in copy mode, where the terminal owns selection). Returns whether
    /// the press started a selection (and thus consumed the click).
    fn try_start_selection(&mut self, view: &mut ViewState, x: u16, y: u16) -> bool {
        if view.copy_mode || self.selection_mode == SelectionMode::Native {
            return false;
        }
        if !rect_contains(view.layout.message_rect, x, y) {
            return false;
        }
        view.selection = Some(Selection::new(x, y));
        self.selecting = true;
        true
    }

    /// Updates the moving end of the in-progress selection, clamping it into the
    /// message area so the highlight and copied rows never leave the
    /// conversation. Returns whether the frame changed.
    fn update_selection(&mut self, view: &mut ViewState, x: u16, y: u16) -> bool {
        let rect = view.layout.message_rect;
        let Some(selection) = view.selection.as_mut() else {
            self.selecting = false;
            return false;
        };
        let cx = x.clamp(rect.x, rect.right().saturating_sub(1));
        let cy = y.clamp(rect.y, rect.bottom().saturating_sub(1));
        selection.cursor = (cx, cy);
        true
    }

    /// Toggles the release-capture copy mode, flipping terminal mouse capture to
    /// match: entering releases capture so the terminal selects natively;
    /// leaving re-enables app-level mouse handling.
    fn toggle_copy_mode(&mut self, view: &mut ViewState) -> Result<(), anyhow::Error> {
        if view.toggle_copy_mode() {
            self.ui.disable_mouse_capture()?;
        } else {
            self.ui.enable_mouse_capture()?;
        }
        Ok(())
    }

    /// Copies the current selection's text to the system clipboard and clears the
    /// selection. Reads the text from the last rendered frame (see
    /// [`Tui::selection_text`](crate::tui::Tui::selection_text)). Clipboard
    /// failures (e.g. a headless box with no display) are logged and surfaced as
    /// a one-line status notice rather than crashing.
    fn yank_selection(&mut self, state: &mut State, view: &mut ViewState) {
        let Some(selection) = view.selection else {
            return;
        };

        let rect = view.layout.message_rect;
        let text = self.ui.selection_text(
            selection.selected_rows(),
            rect.x,
            rect.right().saturating_sub(1),
        );
        let backend = view.focused.as_ref().map(|b| b.backend);
        view.clear_selection();

        if text.is_empty() {
            return;
        }

        let line_count = text.lines().count();
        let notice = match copy_to_clipboard(&text) {
            Ok(()) => {
                let plural = if line_count == 1 { "" } else { "s" };
                format!("Copied {line_count} line{plural} to clipboard")
            }
            Err(err) => {
                log::warn!("clipboard copy failed: {err}");
                format!("Clipboard error: {err}").replace(['\r', '\n'], " ")
            }
        };

        if let Some(backend) = backend {
            state.apply(backend, server_info(notice));
        }
    }

    /// Starts a sidebar resize drag when a left-press lands within +/-1 column of
    /// the split boundary and inside the message area's vertical band. Returns
    /// whether the press was consumed as a drag start. A no-op when no sidebar is
    /// shown (`split_x` is `None`).
    fn try_start_split_drag(&mut self, view: &ViewState, x: u16, y: u16) -> bool {
        let Some(split_x) = view.layout.split_x else {
            return false;
        };
        let msg = view.layout.message_rect;
        let in_band = y >= msg.y && y < msg.y.saturating_add(msg.height);
        let on_boundary = x.abs_diff(split_x) <= 1;
        if in_band && on_boundary {
            self.dragging_split = true;
            return true;
        }
        false
    }

    /// Resizes the sidebar so its left edge follows the cursor: the new width is
    /// the distance from the cursor to the sidebar's right edge, so dragging the
    /// boundary left widens the list and dragging right narrows it. The renderer
    /// clamps the stored value, so unbounded saturating math here is fine.
    fn drag_split(&mut self, view: &mut ViewState, x: u16) -> bool {
        let Some(rect) = view.layout.userlist_rect else {
            return false;
        };
        let right_edge = rect.x.saturating_add(rect.width);
        view.sidebar_width = Some(right_edge.saturating_sub(x));
        true
    }

    /// Resolves a left-click against the most recent render's hit regions.
    /// Returns whether the click changed the frame (and thus needs a repaint).
    fn handle_left_click(
        &mut self,
        state: &mut State,
        view: &mut ViewState,
        x: u16,
        y: u16,
    ) -> bool {
        // A click on a buffer tab switches focus, mirroring the Tab key handler's
        // read-marker dance: advance the marker on the buffer we are leaving, then
        // clear the activity flags on the one we land on.
        if let Some(id) = view.layout.tab_at(x, y) {
            let id = id.clone();
            if let Some(buffer) = state.focused_buffer_mut(view) {
                buffer.advance_read_marker();
            }
            view.focus(id);
            if let Some(buffer) = state.focused_buffer_mut(view) {
                buffer.mark_read();
            }
            return true;
        }

        // A click on a user row opens (or focuses) a query buffer for that member
        // on the focused buffer's backend.
        if let Some(index) = view.layout.member_row_at(x, y) {
            let Some(focused) = view.focused.clone() else {
                return false;
            };
            let Some(nick) = state
                .buffers
                .get(&focused)
                .and_then(|buffer| buffer.members.get(index))
                .map(|member| member.user.name().to_string())
            else {
                return false;
            };
            // Never open a query to ourselves.
            if nick == state.nickname(focused.backend) {
                return false;
            }
            self.focus_buffer(state, view, focused.backend, &nick);
            return true;
        }

        false
    }

    /// Routes a mouse event while the context menu is open. A left-click inside
    /// the menu selects and activates the clicked item; a left-click outside
    /// dismisses it (click-to-dismiss). A right-click anywhere closes the menu.
    /// Always returns `true` because every path changes the frame (an activation,
    /// a dismiss, or - for a click on the border - a swallow that still needs the
    /// menu repainted with its current state).
    fn handle_menu_mouse(
        &mut self,
        state: &mut State,
        view: &mut ViewState,
        event: crossterm::event::MouseEvent,
    ) -> bool {
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(index) = view.menu.item_at(event.column, event.row) {
                    view.menu.selected = index;
                    self.activate_menu(state, view);
                } else if !view.menu.contains(event.column, event.row) {
                    view.menu.close();
                }
                true
            }
            MouseEventKind::Down(MouseButton::Right) => {
                view.menu.close();
                true
            }
            // Swallow every other event (scroll, drag, release) so it cannot
            // reach the buffer underneath while the menu is up.
            _ => true,
        }
    }

    /// Opens a context menu for a right-click on a buffer tab or a user row.
    /// Returns whether a menu was opened (and thus the frame changed). Buffer-tab
    /// menus list the safe actions first; user menus need a focused buffer to
    /// resolve the clicked member's nick.
    fn handle_right_click(
        &mut self,
        state: &mut State,
        view: &mut ViewState,
        x: u16,
        y: u16,
    ) -> bool {
        if let Some(id) = view.layout.tab_at(x, y) {
            let target = MenuTarget::Buffer(id.clone());
            let items = vec![
                MenuItem {
                    label: "Mark read".to_string(),
                    action: MenuAction::MarkRead,
                },
                MenuItem {
                    label: "Leave".to_string(),
                    action: MenuAction::Leave,
                },
                MenuItem {
                    label: "Close buffer".to_string(),
                    action: MenuAction::CloseBuffer,
                },
            ];
            view.menu.open_at(x, y, target, items);
            return true;
        }

        if let Some(index) = view.layout.member_row_at(x, y) {
            let Some(focused) = view.focused.clone() else {
                return false;
            };
            let Some(nick) = state
                .buffers
                .get(&focused)
                .and_then(|buffer| buffer.members.get(index))
                .map(|member| member.user.name().to_string())
            else {
                return false;
            };
            let target = MenuTarget::User {
                backend: focused.backend,
                nick,
            };
            let items = vec![
                MenuItem {
                    label: "Whois".to_string(),
                    action: MenuAction::Whois,
                },
                MenuItem {
                    label: "Open query".to_string(),
                    action: MenuAction::OpenQuery,
                },
                MenuItem {
                    label: "Mention".to_string(),
                    action: MenuAction::Mention,
                },
            ];
            view.menu.open_at(x, y, target, items);
            return true;
        }

        false
    }

    /// Performs the highlighted menu item, translating its [`MenuAction`] and
    /// target into a backend [`Command`] or a local state mutation, then closes
    /// the menu. Returns `true` since activation always changes the frame.
    fn activate_menu(&mut self, state: &mut State, view: &mut ViewState) -> bool {
        match (view.menu.selected_action(), view.menu.target.clone()) {
            (Some(MenuAction::MarkRead), Some(MenuTarget::Buffer(id))) => {
                if let Some(buffer) = state.buffers.get_mut(&id) {
                    buffer.mark_read();
                    buffer.advance_read_marker();
                }
            }
            (Some(MenuAction::Leave), Some(MenuTarget::Buffer(id))) => {
                self.send_to(
                    Some(id.backend),
                    Command::Part {
                        target: id.target.clone(),
                        reason: None,
                    },
                );
            }
            (Some(MenuAction::CloseBuffer), Some(MenuTarget::Buffer(id))) => {
                close_buffer(state, view, &id);
            }
            (Some(MenuAction::Whois), Some(MenuTarget::User { backend, nick })) => {
                self.send_to(Some(backend), Command::Whois { user: nick });
            }
            (Some(MenuAction::OpenQuery), Some(MenuTarget::User { backend, nick })) => {
                self.focus_buffer(state, view, backend, &nick);
            }
            (Some(MenuAction::Mention), Some(MenuTarget::User { nick, .. })) => {
                // Drop the mention into the input line and switch to Insert so the
                // user can keep typing. Prefix a separator when the line is not
                // empty so an existing draft is not run together with the nick.
                let current = self.ui.input().value().to_string();
                let line = if current.is_empty() {
                    format!("{nick}: ")
                } else {
                    format!("{current} {nick}: ")
                };
                self.ui.set_input(&line);
                view.mode = Mode::Insert;
            }
            _ => {}
        }

        view.menu.close();
        true
    }

    /// Returns whether the paste was applied to the input line (Insert mode).
    fn handle_paste(&mut self, view: &ViewState, text: String) -> bool {
        if view.mode != Mode::Insert {
            return false;
        }
        // Collapse CR/LF to a space - a multi-line paste must not send multiple messages.
        for ch in text.chars() {
            let ch = if ch == '\r' || ch == '\n' { ' ' } else { ch };
            self.ui.handle_event(&CrosstermEvent::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
        true
    }

    /// Returns `false` when the command requests application exit (`:q`).
    fn handle_command(
        &mut self,
        state: &mut State,
        view: &mut ViewState,
    ) -> Result<bool, anyhow::Error> {
        view.mode = Mode::Normal;

        let focused = view.focused.clone();
        let backend = focused.as_ref().map(|b| b.backend);
        let target = focused.as_ref().map(|b| b.target.clone());

        let command: Box<[&str]> = self.ui.input().value().splitn(2, ' ').collect();

        match *command {
            ["q" | "quit"] => {
                for handle in &self.backends {
                    let _ = handle.send(Command::Quit { reason: None });
                }
                return Ok(false);
            }
            ["m" | "msg", rest] => {
                if let Some(backend) = backend {
                    match *rest.splitn(2, ' ').collect::<Box<[&str]>>() {
                        [to, message] => {
                            let buffer = self.focus_buffer(state, view, backend, to);
                            if !message.trim().is_empty() {
                                self.send(backend, buffer, message.to_string(), MsgKind::Text);
                            }
                        }
                        [to] => {
                            self.focus_buffer(state, view, backend, to);
                        }
                        _ => {}
                    }
                }
            }
            ["me", message] => {
                if let (Some(backend), Some(target)) = (backend, target) {
                    self.send(backend, target, message.to_string(), MsgKind::Action);
                }
            }
            ["desc" | "describe", rest] => {
                if let Some(backend) = backend {
                    if let [to, message] = *rest.splitn(2, ' ').collect::<Box<[&str]>>() {
                        let buffer = self.focus_buffer(state, view, backend, to);
                        self.send(backend, buffer, message.to_string(), MsgKind::Action);
                    }
                }
            }
            ["notice", rest] => {
                if let Some(backend) = backend {
                    if let [to, message] = *rest.splitn(2, ' ').collect::<Box<[&str]>>() {
                        self.send(
                            backend,
                            TargetId::from(to),
                            message.to_string(),
                            MsgKind::Notice,
                        );
                    }
                }
            }
            ["j" | "join", channel] => self.send_to(
                backend,
                Command::Join {
                    target: TargetId::from(channel),
                },
            ),
            ["p" | "part", channel] => self.send_to(
                backend,
                Command::Part {
                    target: TargetId::from(channel),
                    reason: None,
                },
            ),
            ["n" | "nick", nickname] => self.send_to(
                backend,
                Command::SetNick {
                    nick: nickname.to_string(),
                },
            ),
            ["whois", nickname] => self.send_to(
                backend,
                Command::Whois {
                    user: nickname.to_string(),
                },
            ),
            ["topic", text] => {
                if let Some(target) = target {
                    self.send_to(
                        backend,
                        Command::SetTopic {
                            target,
                            topic: text.to_string(),
                        },
                    );
                }
            }
            ["away"] => self.send_to(backend, Command::Away { message: None }),
            ["away", message] => self.send_to(
                backend,
                Command::Away {
                    message: Some(message.to_string()),
                },
            ),
            ["kick", rest] => {
                if let Some(backend) = backend {
                    // :kick [#channel] <nick> [reason...]
                    let (kick_target, nick_and_rest) = if rest.starts_with('#') {
                        let mut it = rest.splitn(2, ' ');
                        let chan = it.next().unwrap_or("");
                        (Some(TargetId::from(chan)), it.next().unwrap_or(""))
                    } else {
                        (target, rest)
                    };
                    if let Some(t) = kick_target {
                        let mut it = nick_and_rest.splitn(2, ' ');
                        let nick = it.next().unwrap_or("");
                        let reason = it.next().map(str::to_string);
                        if !nick.is_empty() {
                            self.send_to(
                                Some(backend),
                                Command::Kick {
                                    target: t,
                                    user: nick.to_string(),
                                    reason,
                                },
                            );
                        }
                    }
                }
            }
            ["invite", rest] => {
                if let Some(backend) = backend {
                    let parts: Box<[&str]> = rest.splitn(2, ' ').collect();
                    match *parts {
                        [user, channel] => {
                            self.send_to(
                                Some(backend),
                                Command::Invite {
                                    user: user.to_string(),
                                    target: TargetId::from(channel),
                                },
                            );
                        }
                        [user] => {
                            if let Some(t) = target {
                                self.send_to(
                                    Some(backend),
                                    Command::Invite {
                                        user: user.to_string(),
                                        target: t,
                                    },
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
            ["list"] => self.send_to(backend, Command::ListChannels),
            ["verify"] => self.send_to(
                backend,
                Command::Verify(VerifyAction::Request { user: None }),
            ),
            ["verify", arg] => self.send_to(backend, Command::Verify(parse_verify(arg))),
            ["redraw"] => {
                self.ui.redraw()?;
            }
            ["reload"] => {
                self.do_reload(state, backend);
            }
            _ => {}
        }

        Ok(true)
    }

    /// Enqueues a command to a specific backend, if one is focused.
    fn send_to(&self, backend: Option<BackendId>, command: Command) {
        if let Some(handle) = backend.and_then(|id| self.backend(id)) {
            let _ = handle.send(command);
        }
    }

    /// Ensures a buffer exists for `(backend, target)` and focuses it.
    fn focus_buffer(
        &self,
        state: &mut State,
        view: &mut ViewState,
        backend: BackendId,
        target: &str,
    ) -> TargetId {
        let target = TargetId::from(target);
        let buffer = crate::core::BufferId::new(backend, target.clone());
        if let Some(b) = state.focused_buffer_mut(view) {
            b.advance_read_marker();
        }
        state.buffers.entry(buffer.clone()).or_default();
        view.focus(buffer);
        if let Some(b) = state.focused_buffer_mut(view) {
            b.mark_read();
        }
        target
    }

    fn history_up(&mut self) {
        let draft = self.ui.input().value().to_string();
        if let Some(entry) = self.history.step_up(draft) {
            self.ui.set_input(&entry);
        }
    }

    fn history_down(&mut self) {
        if let Some(entry) = self.history.step_down() {
            self.ui.set_input(&entry);
        }
    }

    fn key_code_is_digit(key_code: KeyCode) -> bool {
        matches!(key_code, KeyCode::Char(char) if char.is_ascii_digit())
    }

    fn get_key_code_as_digit(key_code: KeyCode) -> u8 {
        match key_code {
            KeyCode::Char(char) => char.to_digit(10).unwrap_or(0) as u8,
            _ => 0,
        }
    }

    /// Returns `false` to request application exit.
    pub fn handle_event(
        &mut self,
        state: &mut State,
        view: &mut ViewState,
        event: Event,
    ) -> Result<bool, anyhow::Error> {
        match event {
            // Set dirty before the fallible call so an error path still repaints.
            Event::Input(key) => {
                self.dirty = true;
                self.handle_key(state, view, key)
            }
            Event::Mouse(mouse) => {
                self.dirty |= self.handle_mouse(state, view, mouse);
                Ok(true)
            }
            Event::Paste(text) => {
                self.dirty |= self.handle_paste(view, text);
                Ok(true)
            }
            Event::Backend(message) => {
                self.dirty = true;
                self.handle_backend(state, view, message);
                Ok(true)
            }
            Event::Tick => {
                self.dirty |= self.handle_tick(state, view);
                Ok(true)
            }
        }
    }

    fn scroll_up(&self, state: &mut State, view: &ViewState, lines: usize) {
        if let Some(buffer) = state.focused_buffer_mut(view) {
            buffer.scroll_up(lines);
        }
    }

    fn scroll_down(&self, state: &mut State, view: &ViewState, lines: usize) {
        if let Some(buffer) = state.focused_buffer_mut(view) {
            buffer.scroll_down(lines);
        }
    }

    fn handle_key(
        &mut self,
        state: &mut State,
        view: &mut ViewState,
        event: KeyEvent,
    ) -> Result<bool, anyhow::Error> {
        // While the context menu is open it captures the keyboard modally: arrows
        // move the highlight, Enter activates, Esc dismisses, and every other key
        // is swallowed so it cannot reach the buffer or input line underneath.
        // Key events already set `dirty` in `handle_event`, so returning here
        // still repaints.
        if view.menu.open {
            match event.code {
                KeyCode::Up => view.menu.move_up(),
                KeyCode::Down => view.menu.move_down(),
                KeyCode::Enter => {
                    self.activate_menu(state, view);
                }
                KeyCode::Esc => view.menu.close(),
                _ => {}
            }
            return Ok(true);
        }

        let page = (view.viewport_height as usize).max(1);

        match (view.mode, event.code) {
            // Ctrl-L: force a full screen repaint, in any mode. Clears ghosting
            // left by terminals that render a glyph narrower than its Unicode
            // width (e.g. emoji-presentation characters).
            (_, KeyCode::Char('l')) if event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.ui.redraw()?;
            }
            (Mode::Normal, KeyCode::Tab) => {
                if let Some(b) = state.focused_buffer_mut(view) {
                    b.advance_read_marker();
                }
                view.next_buffer(state);
                if let Some(b) = state.focused_buffer_mut(view) {
                    b.mark_read();
                }
            }
            (Mode::Normal, KeyCode::BackTab) => {
                if let Some(b) = state.focused_buffer_mut(view) {
                    b.advance_read_marker();
                }
                view.previous_buffer(state);
                if let Some(b) = state.focused_buffer_mut(view) {
                    b.mark_read();
                }
            }
            (Mode::Normal, code) if Self::key_code_is_digit(code) => {
                if let Some(b) = state.focused_buffer_mut(view) {
                    b.advance_read_marker();
                }
                let index = Self::get_key_code_as_digit(code) as usize;
                view.focus_buffer_index(state, index);
                if let Some(b) = state.focused_buffer_mut(view) {
                    b.mark_read();
                }
            }
            (Mode::Normal, KeyCode::PageUp) => self.scroll_up(state, view, page),
            (Mode::Normal, KeyCode::PageDown) => self.scroll_down(state, view, page),
            (Mode::Normal, KeyCode::Char('u'))
                if event.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.scroll_up(state, view, page / 2)
            }
            (Mode::Normal, KeyCode::Char('d'))
                if event.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.scroll_down(state, view, page / 2)
            }
            (Mode::Normal, KeyCode::Home) => {
                if let Some(buffer) = state.focused_buffer_mut(view) {
                    buffer.scroll_to_top(view.viewport_height as usize);
                }
            }
            (Mode::Normal, KeyCode::End) => {
                if let Some(buffer) = state.focused_buffer_mut(view) {
                    buffer.scroll_to_bottom();
                }
            }
            // Resize the user-list sidebar: shrink, grow, or reset to default.
            (Mode::Normal, KeyCode::Char('<')) => view.shrink_sidebar(2),
            (Mode::Normal, KeyCode::Char('>')) => view.grow_sidebar(2),
            (Mode::Normal, KeyCode::Char('=')) => view.reset_sidebar_width(),
            (Mode::Normal, KeyCode::Char('i')) => view.mode = Mode::Insert,
            (Mode::Normal, KeyCode::Char(':')) => view.mode = Mode::Command,
            // Ctrl-s: toggle release-capture copy mode (mnemonic: select). Hands
            // text selection to the terminal and back; available in both
            // selection modes as the escape hatch.
            (Mode::Normal, KeyCode::Char('s'))
                if event.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.toggle_copy_mode(view)?;
            }
            // Yank the app-level selection to the clipboard: `y`, or Ctrl-c. A
            // no-op when nothing is selected.
            (Mode::Normal, KeyCode::Char('y')) => self.yank_selection(state, view),
            (Mode::Normal, KeyCode::Char('c'))
                if event.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.yank_selection(state, view)
            }
            // Esc in Normal mode leaves copy mode, else clears a selection.
            (Mode::Normal, KeyCode::Esc) => {
                if view.copy_mode {
                    self.toggle_copy_mode(view)?;
                } else {
                    view.clear_selection();
                }
            }
            (Mode::Command | Mode::Insert, KeyCode::Esc) => {
                view.mode = Mode::Normal;
                self.ui.reset_input();
            }
            (Mode::Command, KeyCode::Enter) => {
                let proceed = self.handle_command(state, view)?;
                self.ui.reset_input();
                return Ok(proceed);
            }
            (Mode::Insert, KeyCode::Up) => {
                self.history_up();
            }
            (Mode::Insert, KeyCode::Down) => {
                self.history_down();
            }
            (Mode::Insert, KeyCode::Enter) => {
                let message = self.ui.input().value().to_string();
                if !message.trim().is_empty() {
                    if let Some(buffer) = view.focused.clone() {
                        self.send(
                            buffer.backend,
                            buffer.target,
                            message.clone(),
                            MsgKind::Text,
                        );
                    }
                    self.history.push(message);
                }
                self.ui.reset_input();
            }
            (Mode::Command | Mode::Insert, _) => {
                self.ui.handle_event(&CrosstermEvent::Key(event));
            }
            _ => {}
        }

        Ok(true)
    }

    fn handle_backend(&mut self, state: &mut State, view: &ViewState, message: BackendMessage) {
        let backend = message.backend;

        match message.event {
            BackendEvent::Ready { nickname } => state.set_nickname(backend, nickname),
            BackendEvent::Synced => state.set_synced(backend),
            BackendEvent::Disconnected { reason } => {
                let text = match reason {
                    Some(reason) => format!("Disconnected: {reason}"),
                    None => "Disconnected".to_string(),
                };
                state.apply(backend, server_info(text));
            }
            BackendEvent::Error { message } => {
                state.apply(backend, server_info(format!("Error: {message}")));
            }
            BackendEvent::Event(event) => {
                self.emit_lua_event(state, backend, &event);
                // Anchor the view: if the user has scrolled up in the focused
                // buffer, advance scroll_position so that the same messages
                // stay visible when a new one is appended at the tail.
                let before = state
                    .focused_buffer_mut(view)
                    .map(|b| (b.messages.len(), b.scroll_position));
                state.apply(backend, event);
                if let Some((len_before, pos)) = before {
                    if pos > 0 {
                        if let Some(buffer) = state.focused_buffer_mut(view) {
                            if buffer.messages.len() > len_before {
                                buffer.scroll_position = pos + (buffer.messages.len() - len_before);
                            }
                        }
                    }
                }
                // The user is actively viewing the focused buffer: advance the
                // read marker and clear activity flags so no indicator fires.
                if let Some(b) = state.focused_buffer_mut(view) {
                    b.advance_read_marker();
                    b.mark_read();
                }
            }
        }
    }

    /// Fires the Lua `event` callback for plugins, building the normalized event
    /// table and a backend-bound sender. Best-effort: rendering does not depend
    /// on it, so failures are ignored.
    fn emit_lua_event(&mut self, state: &State, backend: BackendId, event: &ChatEvent) {
        let Some(info) = state.backends.get(&backend).map(|b| b.info.clone()) else {
            return;
        };

        let target = event.target().cloned().unwrap_or_else(TargetId::status);
        let stored = StoredMessage::from_event(event.clone());

        let Ok(table) = to_lua_event(self.lua, &stored, &info, &target, target.as_str()) else {
            return;
        };

        let Ok(sender) = self.sender_table(backend) else {
            return;
        };

        let _ = emit_event(self.lua, EventName::Event, (table, sender));
    }

    fn sender_table(&mut self, backend: BackendId) -> mlua::Result<mlua::Table> {
        if let Some(key) = self.senders.get(&backend) {
            return self.lua.registry_value(key);
        }

        let handle = self
            .backend(backend)
            .ok_or_else(|| mlua::Error::external("unknown backend"))?;
        let table = create_lua_sender(self.lua, handle.sender(), Arc::clone(&self.txn))?;
        let key = self.lua.create_registry_value(&table)?;
        self.senders.insert(backend, key);
        Ok(table)
    }
}

/// Input history for the Insert-mode line editor.
///
/// `entries` holds sent messages newest-last. `index` is `Some(i)` while the
/// user is browsing; `None` means "at the live input". `draft` saves the
/// in-progress text when the user first presses Up, so Down past the last entry
/// restores exactly what they had typed.
#[derive(Debug, Default)]
struct History {
    entries: Vec<String>,
    index: Option<usize>,
    draft: String,
}

impl History {
    /// Adds a sent message. Consecutive duplicates are collapsed.
    /// Resets the browsing position.
    fn push(&mut self, message: String) {
        if self.entries.last().map(String::as_str) != Some(message.as_str()) {
            self.entries.push(message);
        }
        self.index = None;
        self.draft = String::new();
    }

    /// Move to the previous (older) history entry, saving `current_draft` when
    /// entering history for the first time. Returns the entry to load, or `None`
    /// when there is nothing to recall.
    fn step_up(&mut self, current_draft: String) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let new_index = match self.index {
            None => {
                self.draft = current_draft;
                self.entries.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.index = Some(new_index);
        Some(self.entries[new_index].clone())
    }

    /// Move to the next (newer) history entry, or back to the live draft when
    /// already at the most recent entry. Returns the text to load, or `None`
    /// when not currently browsing history.
    fn step_down(&mut self) -> Option<String> {
        let index = self.index?;
        if index + 1 < self.entries.len() {
            let new_index = index + 1;
            self.index = Some(new_index);
            Some(self.entries[new_index].clone())
        } else {
            self.index = None;
            Some(self.draft.clone())
        }
    }
}

/// Maps the argument of `:verify <arg>` to a [`VerifyAction`]. `accept`,
/// `confirm` and `cancel` advance an in-flight verification; anything else is
/// treated as a user id to start verifying.
fn parse_verify(arg: &str) -> VerifyAction {
    match arg.trim() {
        "accept" => VerifyAction::Accept,
        "confirm" => VerifyAction::Confirm,
        "cancel" | "reject" => VerifyAction::Cancel,
        user => VerifyAction::Request {
            user: Some(user.to_string()),
        },
    }
}

/// Removes `id` from the buffer list and refocuses a neighbour if it was the
/// focused buffer. A pure `(State, ViewState)` transition with no `InputHandler`
/// or terminal, so it is unit-testable directly.
///
/// The status buffer is never closeable - it is the backend's home and there is
/// always at least one. `shift_remove` preserves the order of the remaining
/// buffers (unlike `swap_remove`), so the bar does not reshuffle on close. The
/// neighbour is chosen from the removed buffer's former index clamped into the
/// new (shorter) list, which lands on the next buffer to the right, or the new
/// last buffer when the closed one was rightmost.
fn close_buffer(state: &mut State, view: &mut ViewState, id: &crate::core::BufferId) {
    if id.target.is_status() {
        return;
    }

    let index = state.buffers.get_index_of(id);
    let was_focused = view.focused.as_ref() == Some(id);
    state.buffers.shift_remove(id);

    if was_focused {
        let len = state.buffers.len();
        if len == 0 {
            view.focused = None;
            return;
        }
        let neighbour = index.map(|i| i.min(len - 1)).unwrap_or(0);
        view.focus_buffer_index(state, neighbour);
        if let Some(buffer) = state.focused_buffer_mut(view) {
            buffer.mark_read();
        }
    }
}

fn server_info(text: String) -> ChatEvent {
    ChatEvent::ServerInfo {
        target: None,
        from: None,
        code: None,
        text,
        raw: None,
    }
}

/// Whether `(x, y)` falls inside `rect`. Mirrors the helper in `state.rs`; kept
/// local so the mouse paths read terse.
fn rect_contains(rect: ratatui::layout::Rect, x: u16, y: u16) -> bool {
    x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

/// Copies `text` to the system clipboard, mapping every failure to a string so
/// the caller can surface it without an `arboard`-specific type. The
/// [`arboard::Clipboard`] is created per call (cheap, and avoids holding an
/// X11/Wayland connection open for the process lifetime); construction itself
/// can fail on a headless/no-display box, which is why this returns a `Result`.
fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|err| err.to_string())?;
    clipboard
        .set_text(text.to_string())
        .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::BackendInfo;
    use crate::core::{BufferId, MessageBody, MsgKind, Protocol, TargetId, UserRef};

    fn state_with_buffers(channels: &[&str]) -> (State, BackendId) {
        let backend = BackendId(0);
        let mut state = State::new();
        state.register_backend(BackendInfo {
            id: backend,
            protocol: Protocol::Irc,
            name: "test".to_string(),
        });
        for channel in channels {
            state.apply(
                backend,
                ChatEvent::Message {
                    target: TargetId::from(*channel),
                    id: None,
                    sender: UserRef::new("alice"),
                    body: MessageBody::plain("hi"),
                    kind: MsgKind::Text,
                    echo_of: None,
                    time: None,
                },
            );
        }
        (state, backend)
    }

    #[test]
    fn close_buffer_refocuses_a_neighbour() {
        let (mut state, backend) = state_with_buffers(&["#a", "#b"]);
        // Buffers in order: (status), #a, #b.
        let mut view = ViewState::new();
        let a = BufferId::new(backend, "#a");
        view.focus(a.clone());

        close_buffer(&mut state, &mut view, &a);

        assert!(!state.buffers.contains_key(&a), "closed buffer is removed");
        // The former index (1) clamps into the shorter list and lands on #b.
        assert_eq!(
            view.focused.as_ref().unwrap().target.as_str(),
            "#b",
            "focus moves to the next buffer"
        );
    }

    #[test]
    fn close_buffer_refuses_the_status_buffer() {
        let (mut state, backend) = state_with_buffers(&["#a"]);
        let status = BufferId::status(backend);
        let mut view = ViewState::new();
        view.focus(status.clone());

        close_buffer(&mut state, &mut view, &status);

        assert!(
            state.buffers.contains_key(&status),
            "status buffer is never closeable"
        );
        assert_eq!(view.focused, Some(status));
    }

    #[test]
    fn close_unfocused_buffer_leaves_focus_untouched() {
        let (mut state, backend) = state_with_buffers(&["#a", "#b"]);
        let mut view = ViewState::new();
        let a = BufferId::new(backend, "#a");
        let b = BufferId::new(backend, "#b");
        view.focus(b.clone());

        close_buffer(&mut state, &mut view, &a);

        assert!(!state.buffers.contains_key(&a));
        assert_eq!(
            view.focused,
            Some(b),
            "focus stays on the still-open buffer"
        );
    }

    #[test]
    fn history_push_deduplicates_consecutive() {
        let mut h = History::default();
        h.push("hello".to_string());
        h.push("hello".to_string());
        assert_eq!(h.entries.len(), 1);
        h.push("world".to_string());
        assert_eq!(h.entries.len(), 2);
    }

    #[test]
    fn history_push_resets_index() {
        let mut h = History::default();
        h.push("a".to_string());
        h.push("b".to_string());
        h.step_up(String::new());
        assert!(h.index.is_some());
        h.push("c".to_string());
        assert!(h.index.is_none());
    }

    #[test]
    fn history_up_returns_most_recent_first() {
        let mut h = History::default();
        h.push("first".to_string());
        h.push("second".to_string());
        assert_eq!(h.step_up(String::new()).as_deref(), Some("second"));
        assert_eq!(h.step_up(String::new()).as_deref(), Some("first"));
        // Clamped at oldest entry.
        assert_eq!(h.step_up(String::new()).as_deref(), Some("first"));
    }

    #[test]
    fn history_down_restores_draft() {
        let mut h = History::default();
        h.push("msg".to_string());
        h.step_up("draft".to_string());
        assert_eq!(h.step_down().as_deref(), Some("draft"));
        assert!(h.index.is_none());
    }

    #[test]
    fn history_down_returns_none_when_not_browsing() {
        let mut h = History::default();
        h.push("msg".to_string());
        assert_eq!(h.step_down(), None);
    }

    #[test]
    fn history_up_returns_none_when_empty() {
        let mut h = History::default();
        assert_eq!(h.step_up(String::new()), None);
    }

    #[test]
    fn history_cycles_through_all_entries_then_back() {
        let mut h = History::default();
        for msg in ["a", "b", "c"] {
            h.push(msg.to_string());
        }
        // Navigate to oldest.
        h.step_up(String::new());
        h.step_up(String::new());
        h.step_up(String::new());
        assert_eq!(h.index, Some(0));
        // Navigate back to newest.
        h.step_down();
        h.step_down();
        assert_eq!(h.index, Some(2));
        // One more Down returns the draft.
        let result = h.step_down();
        assert!(result.is_some());
        assert!(h.index.is_none());
    }
}
