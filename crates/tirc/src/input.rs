use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crossterm::event::{
    Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind,
};
use mlua::Lua;

use tirc_config::{
    aliases::AliasStore, buffer_order::BufferOrderStore, collect_user_watched_paths,
    reload_lua_theme, ui_prefs::UiPrefsStore, QuickReactions, SelectionMode,
};
use tirc_core::backend::BackendHandle;
use tirc_core::{
    BackendEvent, BackendId, BackendMessage, BufferId, ChatEvent, Command, EventId, MsgKind,
    TargetId, TxnAllocator, VerifyAction, DEBUG_BACKEND,
};
use tirc_lua::runtime::{emit_event, EventName};
use tirc_tui::{parse_bar_id, DecodedImage, PreviewResult, Tui};
use tirc_ui::lua::{create_lua_sender, to_lua_event};
use tirc_ui::ConnectionStatus;

use tirc_ui::commands::{self, BuiltinCmd, Resolution};
use tirc_ui::completion::{self, CompletionEngine, CompletionQuery};
use tirc_ui::{BarHit, MenuAction, MenuItem, MenuTarget, Mode, Selection, State, ViewState};
use tirc_ui::{HistoryState, StoredMessage};

/// Page size of a scroll-triggered history fetch.
const HISTORY_FETCH_LIMIT: u16 = 50;

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
    /// Executed `:` command lines, recalled with Up/Down in Command mode -
    /// vim's cmdline history, separate from the Insert-mode message history.
    command_history: History,
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
    /// The message-area cell where the current left press began, or `None`. A
    /// release on the same cell with no intervening drag is treated as a click
    /// that selects the message under the cursor (enters [`Mode::Select`]).
    /// Cleared when a drag starts or the press is consumed by a pill/split.
    left_press: Option<(u16, u16)>,
    /// The configured default mouse-drag behaviour. In [`SelectionMode::Native`]
    /// a drag does not select in-app; the user relies on the always-available
    /// copy-mode toggle instead.
    selection_mode: SelectionMode,
    /// Quick-reaction config: whether message-select mode is available and the
    /// ordered emoji bound to the number keys `1`..`9` in that mode.
    quick_reactions: QuickReactions,
    /// Persisted `:alias` buffer names, saved to the XDG state dir on change.
    aliases: AliasStore,
    /// Persisted `:bufmove` tab order, saved to the XDG state dir on change.
    buffer_order: BufferOrderStore,
    /// Persisted runtime UI preferences (`:barstyle`), saved on change.
    ui_prefs: UiPrefsStore,
    /// The completion engine queried after every Command/Insert-mode edit;
    /// popup state lives on [`ViewState::completion`].
    completion: CompletionEngine,
    /// Current away message; `Some` while away. Source of truth for the
    /// `:away` no-arg toggle. Not re-sent to backends on reconnect.
    away_message: Option<String>,
}

/// The away message used when `:away` is invoked without one.
const DEFAULT_AWAY_MESSAGE: &str = "AFK";

/// Next away state for `:away <rest>`: text sets it, no-arg toggles (default
/// message when not away, clear when away).
fn next_away_state(current: &Option<String>, rest: &str) -> Option<String> {
    if !rest.is_empty() {
        Some(rest.to_string())
    } else if current.is_some() {
        None
    } else {
        Some(DEFAULT_AWAY_MESSAGE.to_string())
    }
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
        quick_reactions: QuickReactions,
        aliases: AliasStore,
        buffer_order: BufferOrderStore,
        ui_prefs: UiPrefsStore,
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
            command_history: History::default(),
            dirty: true,
            dragging_split: false,
            selecting: false,
            left_press: None,
            selection_mode,
            quick_reactions,
            aliases,
            buffer_order,
            ui_prefs,
            completion: CompletionEngine::new(),
            away_message: None,
        }
    }

    /// Marks the frame as needing a repaint on the next loop iteration.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Records terminal focus (the fallback gate for tmux image drawing when
    /// the pane origin is unknown) and requests a repaint.
    pub fn set_terminal_focus(&mut self, focused: bool) {
        self.ui.set_focused(focused);
        self.dirty = true;
    }

    /// Re-queries the tmux pane origin; repaints when it changed so images
    /// re-emit at their new absolute position.
    pub fn refresh_pane_origin(&mut self) {
        if self.ui.refresh_pane_origin() {
            self.dirty = true;
        }
    }

    /// Whether decoded images are currently cached in the renderer.
    pub fn has_cached_images(&self) -> bool {
        self.ui.has_cached_images()
    }

    /// Forces the next frame to repaint every cell (restores graphics wiped by
    /// a tmux window repaint) and requests that repaint.
    pub fn force_redraw(&mut self) {
        self.ui.force_redraw();
        self.dirty = true;
    }

    /// Feeds a finished background image decode into the renderer's cache. The
    /// caller marks the frame dirty so the newly decoded image is drawn.
    pub fn insert_decoded_image(&mut self, decoded: DecodedImage) {
        self.ui.insert_decoded_image(decoded);
    }

    /// Feeds a finished link-preview fetch into the renderer's cache. The caller
    /// marks the frame dirty so the newly available preview is drawn.
    pub fn insert_link_preview(&mut self, result: PreviewResult) {
        self.ui.insert_link_preview(result);
    }

    /// Persists pending preview-cache changes to disk (debounced by the store);
    /// called on the periodic tick so bursts of results are not written per URL.
    pub fn flush_preview_cache(&self) {
        self.ui.flush_preview_cache();
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
        paths.extend(tirc_lua::builtins::builtin_lua_paths());

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
                // Handlers were cleared and re-registered; re-emit the away
                // state so plugins tracking it (e.g. tirc.plugins.away) recover.
                if self.away_message.is_some() {
                    let _ = emit_event(self.lua, EventName::Away, self.away_message.clone());
                }
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
                    time: None,
                },
            );
        }
    }

    /// Sets a runtime display alias on `buffer` and persists it. A failed save
    /// keeps the in-memory alias and reports the error to the status buffer.
    fn set_alias(&mut self, state: &mut State, buffer: BufferId, name: String) {
        let Some(server) = state
            .backends
            .get(&buffer.backend)
            .map(|b| b.info.name.clone())
        else {
            return;
        };
        let result = self.aliases.set(&server, buffer.target.as_str(), &name);
        let backend = buffer.backend;
        state.user_aliases.insert(buffer, name);
        self.report_save_error(state, backend, "aliases", result);
    }

    /// Removes the runtime alias from `buffer`, revealing the config alias /
    /// display name / raw target underneath, and persists the removal.
    fn remove_alias(&mut self, state: &mut State, buffer: BufferId) {
        if state.user_aliases.remove(&buffer).is_none() {
            return;
        }
        let Some(server) = state
            .backends
            .get(&buffer.backend)
            .map(|b| b.info.name.clone())
        else {
            return;
        };
        let result = self.aliases.remove(&server, buffer.target.as_str());
        self.report_save_error(state, buffer.backend, "aliases", result);
    }

    /// Moves the focused buffer within the tab bar. `arg` is a 1-based
    /// absolute position, or a `+n`/`-n` relative step (clamped to the ends).
    /// The resulting order is snapshotted and persisted.
    fn move_buffer(&mut self, state: &mut State, buffer: BufferId, arg: &str) {
        let Some(from) = state.buffers.get_index_of(&buffer) else {
            return;
        };
        let last = state.buffers.len() - 1;
        let to = if let Some(step) = arg.strip_prefix('+') {
            let Ok(step) = step.parse::<usize>() else {
                return;
            };
            from.saturating_add(step).min(last)
        } else if let Some(step) = arg.strip_prefix('-') {
            let Ok(step) = step.parse::<usize>() else {
                return;
            };
            from.saturating_sub(step)
        } else {
            let Ok(position) = arg.parse::<usize>() else {
                return;
            };
            position.saturating_sub(1).min(last)
        };
        if from == to {
            return;
        }
        state.buffers.move_index(from, to);

        // Snapshot the whole tab order so it restores exactly, and rank every
        // open buffer so later-created buffers sort after the moved ones.
        let snapshot: Vec<(String, String)> = state
            .buffers
            .keys()
            .filter_map(|id| {
                let backend = state.backends.get(&id.backend)?;
                Some((backend.info.name.clone(), id.target.as_str().to_string()))
            })
            .collect();
        state.user_order = state
            .buffers
            .keys()
            .enumerate()
            .map(|(rank, id)| (id.clone(), rank))
            .collect();

        let result = self.buffer_order.set_order(snapshot);
        self.report_save_error(state, buffer.backend, "buffer order", result);
    }

    /// The bar styles the active theme declares via its `buffer_bar_styles`
    /// field, or `None` when the theme declares none.
    fn theme_bar_styles(&self) -> Option<Vec<String>> {
        tirc_lua::runtime::ui_string_list(self.lua, "buffer_bar_styles")
            .filter(|styles| !styles.is_empty())
    }

    /// Sets (or with `reset` clears) the runtime buffer-bar style override and
    /// persists it. Any name is accepted - the theme decides what it means -
    /// but names outside the theme's declared `buffer_bar_styles` are echoed
    /// back so a typo is noticeable.
    fn set_bar_style(
        &mut self,
        state: &mut State,
        view: &mut ViewState,
        backend: Option<BackendId>,
        arg: &str,
    ) {
        if arg == "reset" {
            view.buffer_bar_style = None;
            let result = self.ui_prefs.set_buffer_bar(None);
            if let Some(backend) = backend {
                self.report_save_error(state, backend, "ui prefs", result);
            }
            return;
        }

        view.buffer_bar_style = Some(arg.to_string());
        let result = self.ui_prefs.set_buffer_bar(Some(arg));
        if let Some(styles) = self.theme_bar_styles() {
            if !styles.iter().any(|s| s == arg) {
                self.report_info(
                    state,
                    backend,
                    format!(
                        "Buffer bar style set to '{arg}', which the theme does not declare (theme styles: {}, reset)",
                        styles.join(", ")
                    ),
                );
            }
        }
        if let Some(backend) = backend {
            self.report_save_error(state, backend, "ui prefs", result);
        }
    }

    /// Applies UI actions a Lua callback queued via `tirc.focus_buffer` /
    /// `tirc.select_backend` / `tirc.set_away` (stored on `_tirc.__ui_actions`),
    /// then clears the queue. Unknown buffers/backends and malformed entries are
    /// ignored so a theme bug cannot corrupt view state.
    fn apply_queued_ui_actions(&mut self, state: &mut State, view: &mut ViewState) {
        let Ok(actions) = (|| -> mlua::Result<Option<mlua::Table>> {
            let tirc: mlua::Table = self
                .lua
                .globals()
                .get::<mlua::Table>("package")?
                .get::<mlua::Table>("loaded")?
                .get::<mlua::Table>("_tirc")?;
            let actions = tirc.get::<Option<mlua::Table>>("__ui_actions")?;
            tirc.set("__ui_actions", mlua::Value::Nil)?;
            Ok(actions)
        })() else {
            return;
        };
        let Some(actions) = actions else {
            return;
        };

        for action in actions.sequence_values::<mlua::Table>() {
            let Ok(action) = action else { continue };
            match action.get::<String>("type").as_deref() {
                Ok("focus_buffer") => {
                    let Ok(id) = action.get::<String>("id") else {
                        continue;
                    };
                    let Some(BarHit::Buffer(id)) = parse_bar_id(&id) else {
                        continue;
                    };
                    if !state.buffers.contains_key(&id) {
                        continue;
                    }
                    if let Some(buffer) = state.focused_buffer_mut(view) {
                        buffer.advance_read_marker();
                    }
                    view.focus(id);
                    if let Some(buffer) = state.focused_buffer_mut(view) {
                        buffer.mark_read();
                    }
                }
                Ok("select_backend") => {
                    let Ok(id) = action.get::<usize>("id") else {
                        continue;
                    };
                    let backend = BackendId(id);
                    if state.backends.contains_key(&backend) {
                        view.selected_backend = Some(backend);
                    }
                }
                Ok("set_away") => {
                    // Absent key means nil means "back".
                    let message = action.get::<Option<String>>("message").unwrap_or(None);
                    self.set_away(message);
                }
                _ => {}
            }
        }
    }

    /// Pushes an informational line to `backend`'s status buffer, the channel
    /// used for local command feedback.
    fn report_info(&self, state: &mut State, backend: Option<BackendId>, text: String) {
        if let Some(backend) = backend {
            state.apply(
                backend,
                ChatEvent::ServerInfo {
                    target: None,
                    from: None,
                    code: None,
                    text: text.replace(['\r', '\n'], " "),
                    raw: None,
                    time: None,
                },
            );
        }
    }

    fn report_save_error(
        &self,
        state: &mut State,
        backend: BackendId,
        what: &str,
        result: Result<(), anyhow::Error>,
    ) {
        if let Err(err) = result {
            self.report_info(
                state,
                Some(backend),
                format!("Failed to save {what}: {err}"),
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
                // Free scrolling desyncs the page-jump trail; drop it.
                view.page_trail.clear();
                self.scroll_up(state, view, delta);
                true
            }
            MouseEventKind::ScrollDown => {
                view.page_trail.clear();
                self.scroll_down(state, view, delta);
                true
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // A fresh press drops any prior selection; it is re-established
                // below when the press starts a new one. Clearing here also
                // repaints away a stale highlight when the click does something
                // else (a tab switch, a split-drag, an empty click).
                let had_selection = view.selection.take().is_some();
                // A press on the split boundary begins a resize drag; a press on a
                // reaction pill toggles it. Both consume the press and are never a
                // click-to-select candidate.
                if self.try_start_split_drag(view, event.column, event.row)
                    || self.try_reaction_click(state, view, event.column, event.row)
                {
                    self.left_press = None;
                    return true;
                }
                // Remember the press cell so a release without a drag can be
                // treated as a click that selects the message under the cursor. A
                // press in the message area also begins a text selection;
                // otherwise fall through to tab/user-row handling. `had_selection`
                // keeps the frame repainting when only the cleared highlight
                // changed.
                self.left_press = Some((event.column, event.row));
                self.try_start_selection(view, event.column, event.row)
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
                // Any movement makes this a drag, not a click.
                self.left_press = None;
                self.update_selection(view, event.column, event.row)
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.dragging_split = false;
                // Stop extending the selection but keep it visible so the user
                // can yank it. Repaint only if a drag was actually in progress.
                let was_selecting = self.selecting;
                self.selecting = false;
                // A press+release on the same cell with no drag is a click: drop
                // any zero-length text selection it created and select the message
                // under the cursor (entering select mode), when there is one.
                if self.left_press.take() == Some((event.column, event.row)) {
                    if view
                        .selection
                        .map(|s| s.anchor == s.cursor)
                        .unwrap_or(false)
                    {
                        view.clear_selection();
                    }
                    let selected = self.try_select_message(state, view, event.column, event.row);
                    return selected || was_selecting;
                }
                was_selecting
            }
            MouseEventKind::Moved => handle_mouse_moved(view, event.column, event.row),
            _ => false,
        }
    }

    /// Handles a left-click on a reaction pill: toggles the local user's reaction
    /// (remove if already `mine`, otherwise add) by sending [`Command::React`] to
    /// the focused buffer's backend. Returns whether the click hit a pill (and so
    /// must not fall through to text selection).
    fn try_reaction_click(&mut self, state: &State, view: &ViewState, x: u16, y: u16) -> bool {
        let Some(hit) = view.layout.reaction_at(x, y).cloned() else {
            return false;
        };
        self.toggle_reaction(state, view, hit.event_id, hit.key);
        true
    }

    /// Toggles the local user's `key` reaction on the message identified by
    /// `event_id` in the focused buffer: removes it if already `mine`, otherwise
    /// adds it, by sending [`Command::React`] to the focused backend. Shared by
    /// reaction-pill clicks and the select-mode number keys.
    fn toggle_reaction(&self, state: &State, view: &ViewState, event_id: EventId, key: String) {
        let Some(focused) = view.focused.clone() else {
            return;
        };
        let mine = state
            .buffers
            .get(&focused)
            .and_then(|buffer| {
                buffer
                    .messages
                    .iter()
                    .find(|m| m.event_id() == Some(&event_id))
            })
            .and_then(|message| message.reactions.get(&key))
            .map(|reaction| reaction.mine)
            .unwrap_or(false);

        self.send_to(
            Some(focused.backend),
            Command::React {
                target: focused.target,
                id: event_id,
                key,
                add: !mine,
            },
        );
    }

    /// Number of messages in the focused buffer, or 0 when nothing is focused.
    fn focused_message_count(&self, state: &State, view: &ViewState) -> usize {
        view.focused
            .as_ref()
            .and_then(|id| state.buffers.get(id))
            .map(|buffer| buffer.messages.len())
            .unwrap_or(0)
    }

    /// Resolves the currently selected message's server event id, if any. `None`
    /// when nothing is selected or the selected message has no id (e.g. IRC lines
    /// or a not-yet-confirmed echo), in which case it cannot be reacted to.
    fn selected_event_id(&self, state: &State, view: &ViewState) -> Option<EventId> {
        let index = view.selected_message?;
        let buffer = state.buffers.get(view.focused.as_ref()?)?;
        buffer.message_from_newest(index)?.event_id().cloned()
    }

    /// Enters message-select mode with the newest message selected, scrolling it
    /// into view. A no-op when quick reactions are disabled or the focused buffer
    /// has no messages.
    fn enter_select_mode(&mut self, state: &mut State, view: &mut ViewState) {
        if !self.quick_reactions.enabled {
            return;
        }
        let len = self.focused_message_count(state, view);
        if len == 0 {
            return;
        }
        view.select_newest(len);
        view.mode = Mode::Select;
        self.ensure_selection_visible(state, view);
    }

    /// Leaves message-select mode, dropping the selection and returning to Normal.
    fn leave_select_mode(&mut self, view: &mut ViewState) {
        view.mode = Mode::Normal;
        view.clear_message_selection();
    }

    /// Moves the message selection one message older (`older`) or newer, then
    /// scrolls so the selection stays visible.
    fn select_move(&mut self, state: &mut State, view: &mut ViewState, older: bool) {
        let len = self.focused_message_count(state, view);
        if older {
            view.select_older(len);
        } else {
            view.select_newer();
        }
        self.ensure_selection_visible(state, view);
    }

    /// Scrolls the focused buffer so the selected message is on screen. A no-op
    /// when nothing is selected.
    fn ensure_selection_visible(&mut self, state: &mut State, view: &ViewState) {
        let Some(index) = view.selected_message else {
            return;
        };
        let viewport = view.viewport_height as usize;
        if let Some(buffer) = state.focused_buffer_mut(view) {
            buffer.ensure_message_visible(index, viewport);
        }
    }

    /// Applies the quick reaction bound to `digit` (`1`..`9`) to the selected
    /// message, toggling it. A no-op for `0`, an out-of-range digit, or a selected
    /// message that has no server event id to react to.
    fn apply_quick_reaction(&mut self, state: &State, view: &ViewState, digit: u8) {
        if digit == 0 {
            return;
        }
        let Some(emoji) = self
            .quick_reactions
            .emojis
            .get((digit - 1) as usize)
            .cloned()
        else {
            return;
        };
        let Some(event_id) = self.selected_event_id(state, view) else {
            return;
        };
        self.toggle_reaction(state, view, event_id, emoji);
    }

    /// Selects the message under `(x, y)` (from the last render's hit map) and
    /// enters select mode. Returns whether a message was selected. Inert when
    /// quick reactions are disabled.
    fn try_select_message(
        &mut self,
        state: &mut State,
        view: &mut ViewState,
        x: u16,
        y: u16,
    ) -> bool {
        if !self.quick_reactions.enabled {
            return false;
        }
        let Some(index) = view.layout.message_at(x, y) else {
            return false;
        };
        view.selected_message = Some(index);
        view.mode = Mode::Select;
        self.ensure_selection_visible(state, view);
        true
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
    /// [`Tui::selection_text`](tirc_tui::Tui::selection_text)). Clipboard
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
        // clear the activity flags on the one we land on. Backend tabs (multi-row
        // themes) either select the backend's row or focus its last-viewed buffer.
        if let Some(hit) = view.layout.tab_at(x, y) {
            let id = match hit.clone() {
                BarHit::Buffer(id) => {
                    // A stale id from a theme bug must not create a buffer.
                    if !state.buffers.contains_key(&id) {
                        return false;
                    }
                    id
                }
                BarHit::Backend {
                    backend,
                    select_only: true,
                } => {
                    view.selected_backend = Some(backend);
                    return true;
                }
                BarHit::Backend {
                    backend,
                    select_only: false,
                } => view
                    .last_focused_per_backend
                    .get(&backend)
                    .filter(|id| state.buffers.contains_key(*id))
                    .cloned()
                    .unwrap_or_else(|| BufferId::status(backend)),
                BarHit::Custom(id) => {
                    // Theme-defined element: hand the id back to the theme's
                    // handler, then apply any UI actions it queued.
                    if let Some(Err(err)) =
                        tirc_lua::runtime::call_formatter(self.lua, "on_bar_click", id)
                    {
                        log::warn!("on_bar_click failed: {err}");
                    }
                    self.apply_queued_ui_actions(state, view);
                    return true;
                }
            };
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
        // A right-click on a hyperlink takes precedence over message-row
        // selection: open a menu to copy or open the URL under the cursor.
        if let Some(url) = view.layout.link_at(x, y) {
            let target = MenuTarget::Link(url.to_string());
            let items = vec![
                MenuItem {
                    label: "Open link".to_string(),
                    action: MenuAction::OpenLink,
                },
                MenuItem {
                    label: "Copy link".to_string(),
                    action: MenuAction::CopyLink,
                },
            ];
            view.menu.open_at(x, y, target, items);
            return true;
        }

        // Only buffer tabs get a context menu: none of the actions below is
        // well-defined for a backend tab, so those are deliberately ignored.
        if let Some(BarHit::Buffer(id)) = view.layout.tab_at(x, y) {
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
            (Some(MenuAction::CopyLink), Some(MenuTarget::Link(url))) => {
                let notice = match copy_to_clipboard(&url) {
                    Ok(()) => "Copied link to clipboard".to_string(),
                    Err(err) => {
                        log::warn!("clipboard copy failed: {err}");
                        format!("Clipboard error: {err}").replace(['\r', '\n'], " ")
                    }
                };
                self.notify(state, view, notice);
            }
            (Some(MenuAction::OpenLink), Some(MenuTarget::Link(url))) => {
                if let Err(err) = open::that(&url) {
                    log::warn!("could not open link {url}: {err}");
                    let notice = format!("Could not open link: {err}").replace(['\r', '\n'], " ");
                    self.notify(state, view, notice);
                }
            }
            _ => {}
        }

        view.menu.close();
        true
    }

    /// Surfaces a one-line status notice on the focused buffer's backend (a
    /// server info line), matching how [`Self::yank_selection`] reports results.
    /// A no-op when nothing is focused.
    fn notify(&self, state: &mut State, view: &ViewState, notice: String) {
        if let Some(backend) = view.focused.as_ref().map(|b| b.backend) {
            state.apply(backend, server_info(notice));
        }
    }

    /// Returns whether the paste was applied to the input line (Insert mode).
    /// Re-derives the completion popup from the current input and cursor.
    /// Called after every Command/Insert-mode edit; `force` (Tab) waives the
    /// trigger's minimum query length so the full candidate list opens.
    /// Typing a closing `:` after an exact emoji shortcode auto-accepts it
    /// without opening the popup (`:smile:` just works).
    fn refresh_completion(&mut self, state: &State, view: &mut ViewState, force: bool) {
        if view.mode == Mode::Insert {
            let (value, cursor) = (self.ui.input().value(), self.ui.input().cursor());
            if let Some((span, insert)) = completion::closing_sigil_accept(value, cursor) {
                let (value, cursor) = completion::splice(value, span, &insert);
                self.ui.set_input_with_cursor(value, cursor);
                view.completion.close();
                return;
            }
        }

        let query = CompletionQuery {
            mode: view.mode,
            value: self.ui.input().value(),
            cursor: self.ui.input().cursor(),
            force,
            state: Some(state),
            focused: view.focused.as_ref(),
        };
        match self.completion.query(&query, self.lua) {
            Some((span, items)) => view.completion.show(span, items),
            None => view.completion.close(),
        }
    }

    /// Splices the highlighted completion item into the input over the
    /// popup's trigger span and closes the popup.
    fn accept_completion(&mut self, view: &mut ViewState) {
        if let Some(item) = view.completion.selected_item() {
            let (value, cursor) =
                completion::splice(self.ui.input().value(), view.completion.span, &item.insert);
            self.ui.set_input_with_cursor(value, cursor);
        }
        view.completion.close();
    }

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
    ///
    /// The accepted command set is the registry in [`tirc_ui::commands`]; the
    /// name is resolved vim-style (exact name or alias, then unique prefix)
    /// and the matched spec's [`BuiltinCmd`] selects the handler arm below.
    fn handle_command(
        &mut self,
        state: &mut State,
        view: &mut ViewState,
    ) -> Result<bool, anyhow::Error> {
        view.mode = Mode::Normal;

        let focused = view.focused.clone();
        let backend = focused.as_ref().map(|b| b.backend);
        let target = focused.as_ref().map(|b| b.target.clone());

        let line = self.ui.input().value().trim().to_string();
        if line.is_empty() {
            return Ok(true);
        }
        // Recorded before execution - like vim, failed commands stay
        // recallable for fixing up.
        self.command_history.push(line.clone());

        let (name, rest) = commands::split_line(&line);
        let lua_names = tirc_lua::runtime::user_command_names(self.lua);
        let spec = match commands::resolve(name, &lua_names) {
            Resolution::Builtin(spec) => spec,
            Resolution::Lua(name) => {
                self.run_lua_command(state, view, backend, &name, rest);
                return Ok(true);
            }
            Resolution::Ambiguous(candidates) => {
                self.report_info(
                    state,
                    backend,
                    format!("Ambiguous command: {name} ({})", candidates.join(", ")),
                );
                return Ok(true);
            }
            Resolution::Unknown => {
                self.report_info(state, backend, format!("Not a client command: {name}"));
                return Ok(true);
            }
        };
        if let Err(message) = commands::check_nargs(spec, rest) {
            self.report_info(state, backend, message);
            return Ok(true);
        }

        match spec.cmd {
            BuiltinCmd::Quit => {
                for handle in &self.backends {
                    let _ = handle.send(Command::Quit { reason: None });
                }
                return Ok(false);
            }
            BuiltinCmd::Msg => {
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
            BuiltinCmd::Me => {
                if let (Some(backend), Some(target)) = (backend, target) {
                    self.send(backend, target, rest.to_string(), MsgKind::Action);
                }
            }
            BuiltinCmd::Describe => {
                if let Some(backend) = backend {
                    if let [to, message] = *rest.splitn(2, ' ').collect::<Box<[&str]>>() {
                        let buffer = self.focus_buffer(state, view, backend, to);
                        self.send(backend, buffer, message.to_string(), MsgKind::Action);
                    }
                }
            }
            BuiltinCmd::Notice => {
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
            BuiltinCmd::Join => {
                self.send_to(
                    backend,
                    Command::Join {
                        target: TargetId::from(rest),
                    },
                );
            }
            BuiltinCmd::Part => {
                self.send_to(
                    backend,
                    Command::Part {
                        target: TargetId::from(rest),
                        reason: None,
                    },
                );
            }
            BuiltinCmd::Nick => {
                self.send_to(
                    backend,
                    Command::SetNick {
                        nick: rest.to_string(),
                    },
                );
            }
            BuiltinCmd::Whois => {
                self.send_to(
                    backend,
                    Command::Whois {
                        user: rest.to_string(),
                    },
                );
            }
            BuiltinCmd::Topic => {
                if let Some(target) = target {
                    self.send_to(
                        backend,
                        Command::SetTopic {
                            target,
                            topic: rest.to_string(),
                        },
                    );
                }
            }
            BuiltinCmd::Away => {
                let message = next_away_state(&self.away_message, rest);
                let info = match &message {
                    Some(m) => format!("Away: {m}"),
                    None => "No longer away".to_string(),
                };
                self.set_away(message);
                self.report_info(state, backend, info);
            }
            BuiltinCmd::Kick => {
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
            BuiltinCmd::Invite => {
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
            BuiltinCmd::Alias => {
                if let Some(id) = focused.clone() {
                    self.set_alias(state, id, rest.trim().to_string());
                }
            }
            BuiltinCmd::Unalias => {
                if let Some(id) = focused.clone() {
                    self.remove_alias(state, id);
                }
            }
            BuiltinCmd::Bufmove => {
                if let Some(id) = focused.clone() {
                    let arg = rest.trim().to_string();
                    self.move_buffer(state, id, &arg);
                }
            }
            BuiltinCmd::Barstyle => {
                if rest.is_empty() {
                    let current = view
                        .buffer_bar_style
                        .clone()
                        .map(|s| format!("{s} (override; ':barstyle reset' to clear)"))
                        .unwrap_or_else(|| "theme default".to_string());
                    let styles = self
                        .theme_bar_styles()
                        .map(|styles| styles.join(", "))
                        .unwrap_or_else(|| "none declared by the theme".to_string());
                    self.report_info(
                        state,
                        backend,
                        format!("Buffer bar style: {current}. Theme styles: {styles}"),
                    );
                } else {
                    let arg = rest.trim().to_string();
                    self.set_bar_style(state, view, backend, &arg);
                }
            }
            BuiltinCmd::List => {
                self.send_to(backend, Command::ListChannels);
            }
            BuiltinCmd::Verify => {
                if rest.is_empty() {
                    self.send_to(
                        backend,
                        Command::Verify(VerifyAction::Request { user: None }),
                    );
                } else {
                    self.send_to(backend, Command::Verify(parse_verify(rest)));
                }
            }
            BuiltinCmd::Redraw => {
                self.ui.redraw()?;
            }
            BuiltinCmd::Debug => {
                state.show_debug_buffer();
                self.focus_buffer(state, view, DEBUG_BACKEND, TargetId::STATUS);
            }
            BuiltinCmd::Reload => {
                self.do_reload(state, backend);
            }
        }

        Ok(true)
    }

    /// Executes a Lua user command registered via `tirc.create_command`. The
    /// handler receives a ctx table (`name`, `args`, `fargs`, `buffer`,
    /// `backend`) and, when a backend is focused, the same sender table
    /// `tirc.on('event', ...)` handlers get. Queued UI actions (e.g.
    /// `tirc.focus_buffer`) are applied afterwards; errors are reported to the
    /// status buffer and the log.
    fn run_lua_command(
        &mut self,
        state: &mut State,
        view: &mut ViewState,
        backend: Option<BackendId>,
        name: &str,
        rest: &str,
    ) {
        let Some(spec) = tirc_lua::runtime::user_command_spec(self.lua, name) else {
            return;
        };
        let nargs: String = spec.get("nargs").unwrap_or_else(|_| "0".to_string());
        let arity_error = match nargs.as_str() {
            "0" if !rest.is_empty() => {
                Some(format!("Trailing characters: :{name} takes no arguments"))
            }
            "1" | "+" if rest.is_empty() => Some(format!("Argument required for :{name}")),
            _ => None,
        };
        if let Some(message) = arity_error {
            self.report_info(state, backend, message);
            return;
        }
        let Ok(func) = spec.get::<mlua::Function>("fn") else {
            return;
        };

        let result = (|| -> mlua::Result<()> {
            let ctx = self.lua.create_table()?;
            ctx.set("name", name)?;
            ctx.set("args", rest)?;
            ctx.set("fargs", rest.split_whitespace().collect::<Vec<_>>())?;
            if let Some(focused) = view.focused.as_ref() {
                ctx.set("buffer", focused.target.as_str())?;
                ctx.set("backend", focused.backend.0)?;
            }
            let sender = match backend {
                Some(backend) => mlua::Value::Table(self.sender_table(backend)?),
                None => mlua::Value::Nil,
            };
            func.call::<()>((ctx, sender))
        })();
        if let Err(err) = result {
            log::error!(target: "tirc::lua", "command :{name} failed: {err}");
            let first_line = err.to_string().replace(['\r', '\n'], " ");
            self.report_info(
                state,
                backend,
                format!("Error executing :{name}: {first_line}"),
            );
        }

        self.apply_queued_ui_actions(state, view);
    }

    /// Enqueues a command to a specific backend, if one is focused. Returns
    /// whether the command was actually handed to a backend, for callers whose
    /// state must not advance on a dropped send (e.g. history fetches).
    fn send_to(&self, backend: Option<BackendId>, command: Command) -> bool {
        match backend.and_then(|id| self.backend(id)) {
            Some(handle) => handle.send(command).is_ok(),
            None => false,
        }
    }

    /// Completes a finished host task (`tirc.spawn`/`tirc.fetch`): runs the
    /// Lua callback on this thread, applies any UI actions it queued, and
    /// requests a repaint since handlers commonly change render-relevant state.
    pub fn on_host_task(
        &mut self,
        state: &mut State,
        view: &mut ViewState,
        message: tirc_lua::host_tasks::HostTaskMessage,
    ) {
        if let Err(err) = tirc_lua::host_tasks::deliver_host_task(self.lua, message) {
            log::warn!("host task callback failed: {err}");
        }
        self.apply_queued_ui_actions(state, view);
        self.mark_dirty();
    }

    /// Applies a new away state: broadcasts [`Command::Away`] to every backend
    /// (native away where the protocol supports it) and fires the Lua `away`
    /// event so plugins can track it. No-op when the state is unchanged.
    fn set_away(&mut self, message: Option<String>) {
        if self.away_message == message {
            return;
        }
        self.away_message = message.clone();
        for handle in &self.backends {
            let _ = handle.send(Command::Away {
                message: message.clone(),
            });
        }
        let _ = emit_event(self.lua, EventName::Away, message);
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
        let buffer = tirc_core::BufferId::new(backend, target.clone());
        if let Some(b) = state.focused_buffer_mut(view) {
            b.advance_read_marker();
        }
        state.ensure_buffer(buffer.clone());
        view.focus(buffer);
        if let Some(b) = state.focused_buffer_mut(view) {
            b.mark_read();
        }
        target
    }

    /// The history matching the mode: Command recalls executed `:` lines,
    /// everything else the sent messages.
    fn history_for(&mut self, mode: Mode) -> &mut History {
        if mode == Mode::Command {
            &mut self.command_history
        } else {
            &mut self.history
        }
    }

    fn history_up(&mut self, mode: Mode) {
        let draft = self.ui.input().value().to_string();
        if let Some(entry) = self.history_for(mode).step_up(draft) {
            self.ui.set_input(&entry);
        }
    }

    fn history_down(&mut self, mode: Mode) {
        if let Some(entry) = self.history_for(mode).step_down() {
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

    /// One page of scrolling in message indices: the index span of the messages
    /// actually visible in the last rendered frame. `scroll_position` counts
    /// messages while the screen is rows, so a fixed row count is a poor page:
    /// wrapped lines, inline images, and link previews make a message taller
    /// than one row (a row-count page overshoots several screens), and messages
    /// the theme renders as nothing still occupy indices (it undershoots).
    /// Falls back to the viewport row count before the first frame.
    fn page_step(view: &ViewState) -> usize {
        let indices = || view.layout.message_rows.iter().map(|(_, index)| *index);
        match (indices().min(), indices().max()) {
            (Some(min), Some(max)) => max - min + 1,
            _ => (view.viewport_height as usize).max(1),
        }
    }

    /// PageUp: jump up one screenful, remembering the position the jump left
    /// from so PageDown can return exactly (see [`ViewState::page_trail`]; the
    /// step is measured on the screen being left, so the reverse key cannot
    /// re-derive it). When the newest trail entry lies *above* the current
    /// position - left behind by an earlier PageDown - return to it instead.
    fn page_up(&self, state: &mut State, view: &mut ViewState, step: usize) {
        let Some(focused) = view.focused.clone() else {
            return;
        };
        let Some(current) = state.buffers.get(&focused).map(|b| b.scroll_position) else {
            return;
        };
        if let Some(&back) = view.page_trail.last().filter(|&&pos| pos > current) {
            view.page_trail.pop();
            if let Some(buffer) = state.buffers.get_mut(&focused) {
                let max = buffer.messages.len().saturating_sub(1);
                buffer.scroll_position = back.min(max);
            }
            self.maybe_fetch_history(state, view);
            return;
        }
        view.page_trail.push(current);
        self.scroll_up(state, view, step);
        // A jump that went nowhere (already pinned at the top) leaves no trail.
        if state.buffers.get(&focused).map(|b| b.scroll_position) == Some(current) {
            view.page_trail.pop();
        }
    }

    /// PageDown: the mirror of [`Self::page_up`]. Returns to the newest trail
    /// entry *below* the current position when one exists (an earlier PageUp's
    /// origin), else jumps down one screenful and leaves a trail entry.
    fn page_down(&self, state: &mut State, view: &mut ViewState, step: usize) {
        let Some(focused) = view.focused.clone() else {
            return;
        };
        let Some(current) = state.buffers.get(&focused).map(|b| b.scroll_position) else {
            return;
        };
        if let Some(&back) = view.page_trail.last().filter(|&&pos| pos < current) {
            view.page_trail.pop();
            if let Some(buffer) = state.buffers.get_mut(&focused) {
                buffer.scroll_position = back;
            }
            return;
        }
        view.page_trail.push(current);
        self.scroll_down(state, view, step);
        // A jump that went nowhere (already at the bottom) leaves no trail.
        if state.buffers.get(&focused).map(|b| b.scroll_position) == Some(current) {
            view.page_trail.pop();
        }
    }

    fn scroll_up(&self, state: &mut State, view: &ViewState, lines: usize) {
        if let Some(focused) = view.focused.clone() {
            if let Some(buffer) = state.buffers.get_mut(&focused) {
                let before = buffer.scroll_position;
                buffer.scroll_up(lines);
                // Cap at the renderer's top-of-history clamp so hitting the top
                // keeps a full screen instead of shrinking to a lone message.
                // `max(before)` keeps an already-overshot position (a stale
                // clamp) from being yanked *down* by an up-scroll.
                if let Some(top) = view.top_scroll_for(&focused, buffer.messages.len()) {
                    buffer.scroll_position = buffer.scroll_position.min(top.max(before));
                }
            }
        }
        self.maybe_fetch_history(state, view);
    }

    /// Sends [`Command::FetchHistory`] for the focused buffer when the user has
    /// scrolled to within a viewport of the oldest loaded message. One fetch in
    /// flight per buffer ([`HistoryState::Fetching`]); exhausted buffers are
    /// never re-asked.
    fn maybe_fetch_history(&self, state: &mut State, view: &ViewState) {
        let Some(focused) = view.focused.clone() else {
            return;
        };
        // Only after initial sync and while connected: during initial backfill
        // the buffer is still filling, and a disconnected backend drops
        // commands, so the completion event would never arrive.
        let ready = state
            .backends
            .get(&focused.backend)
            .is_some_and(|b| b.synced && b.connection_status == ConnectionStatus::Connected);
        if !ready {
            return;
        }
        let Some(buffer) = state.buffers.get_mut(&focused) else {
            return;
        };
        if !buffer.wants_history_fetch(view.viewport_height as usize) {
            return;
        }
        let before = buffer
            .messages
            .first()
            .map(|m| m.time.with_timezone(&chrono::Utc));
        let before_id = buffer.oldest_event_id().cloned();
        // Flip to Fetching only when the command was actually enqueued: a
        // dropped send would never produce the HistoryFetched completion that
        // returns the buffer to Idle, silently disabling further fetches.
        let sent = self.send_to(
            Some(focused.backend),
            Command::FetchHistory {
                target: focused.target.clone(),
                before,
                before_id,
                limit: HISTORY_FETCH_LIMIT,
            },
        );
        if sent {
            if let Some(buffer) = state.buffers.get_mut(&focused) {
                buffer.history = HistoryState::Fetching;
            }
        }
    }

    fn scroll_down(&self, state: &mut State, view: &ViewState, lines: usize) {
        if let Some(focused) = view.focused.clone() {
            if let Some(buffer) = state.buffers.get_mut(&focused) {
                // Snap an overshot position back to the top-of-history clamp
                // first, so the first down-scroll moves the view instead of
                // consuming invisible offset above the full-screen top.
                if let Some(top) = view.top_scroll_for(&focused, buffer.messages.len()) {
                    buffer.scroll_position = buffer.scroll_position.min(top);
                }
                buffer.scroll_down(lines);
            }
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

        // While the completion popup is open it captures only navigation,
        // accept, and dismiss keys; everything else falls through so typing
        // keeps editing the line and live-refiltering the popup.
        if view.completion.open && matches!(view.mode, Mode::Command | Mode::Insert) {
            let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
            match event.code {
                KeyCode::Tab | KeyCode::Down => {
                    view.completion.move_down();
                    return Ok(true);
                }
                KeyCode::Char('n') if ctrl => {
                    view.completion.move_down();
                    return Ok(true);
                }
                KeyCode::BackTab | KeyCode::Up => {
                    view.completion.move_up();
                    return Ok(true);
                }
                KeyCode::Char('p') if ctrl => {
                    view.completion.move_up();
                    return Ok(true);
                }
                KeyCode::Enter => {
                    self.accept_completion(view);
                    return Ok(true);
                }
                KeyCode::Esc => {
                    view.completion.close();
                    return Ok(true);
                }
                _ => {}
            }
        }

        let page = Self::page_step(view);

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
            (Mode::Normal, KeyCode::PageUp) => self.page_up(state, view, page),
            (Mode::Normal, KeyCode::PageDown) => self.page_down(state, view, page),
            (Mode::Normal, KeyCode::Char('u'))
                if event.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                view.page_trail.clear();
                self.scroll_up(state, view, (page / 2).max(1))
            }
            (Mode::Normal, KeyCode::Char('d'))
                if event.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                view.page_trail.clear();
                self.scroll_down(state, view, (page / 2).max(1))
            }
            (Mode::Normal, KeyCode::Home) => {
                view.page_trail.clear();
                if let Some(focused) = view.focused.clone() {
                    if let Some(buffer) = state.buffers.get_mut(&focused) {
                        // The renderer's clamp is exact (wrapped heights);
                        // scroll_to_top's rows-as-messages estimate is the
                        // fallback until a frame near the top computes it.
                        match view.top_scroll_for(&focused, buffer.messages.len()) {
                            Some(top) => buffer.scroll_position = top,
                            None => buffer.scroll_to_top(view.viewport_height as usize),
                        }
                    }
                }
                self.maybe_fetch_history(state, view);
            }
            (Mode::Normal, KeyCode::End) => {
                view.page_trail.clear();
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
            // Enter message-select mode to react to a message with the quick
            // reactions. Inert when the feature is disabled or the buffer is empty.
            (Mode::Normal, KeyCode::Char('v')) => self.enter_select_mode(state, view),
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
            // Message-select mode: navigate the highlight and react by number.
            (Mode::Select, KeyCode::Char('k') | KeyCode::Up) => self.select_move(state, view, true),
            (Mode::Select, KeyCode::Char('j') | KeyCode::Down) => {
                self.select_move(state, view, false)
            }
            (Mode::Select, code) if Self::key_code_is_digit(code) => {
                let digit = Self::get_key_code_as_digit(code);
                self.apply_quick_reaction(state, view, digit);
            }
            (Mode::Select, KeyCode::Esc) => self.leave_select_mode(view),
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
                view.completion.close();
                self.ui.reset_input();
            }
            // With the popup closed, Tab force-opens command completion with
            // the full candidate list.
            (Mode::Command, KeyCode::Tab) => {
                self.refresh_completion(state, view, true);
            }
            (Mode::Command, KeyCode::Enter) => {
                view.completion.close();
                let proceed = self.handle_command(state, view)?;
                self.ui.reset_input();
                return Ok(proceed);
            }
            (Mode::Command | Mode::Insert, KeyCode::Up) => {
                self.history_up(view.mode);
                view.completion.close();
            }
            (Mode::Command | Mode::Insert, KeyCode::Down) => {
                self.history_down(view.mode);
                view.completion.close();
            }
            (Mode::Insert, KeyCode::Enter) => {
                view.completion.close();
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
                self.refresh_completion(state, view, false);
            }
            _ => {}
        }

        Ok(true)
    }

    fn handle_backend(&mut self, state: &mut State, view: &ViewState, message: BackendMessage) {
        let backend = message.backend;

        match message.event {
            BackendEvent::Ready { nickname } => {
                state.set_nickname(backend, nickname);
                state.set_connection_status(backend, ConnectionStatus::Connected);
            }
            BackendEvent::Synced => state.set_synced(backend),
            BackendEvent::Disconnected { reason } => {
                let text = match reason {
                    Some(reason) => format!("Disconnected: {reason}"),
                    None => "Disconnected".to_string(),
                };
                state.apply(backend, server_info(text));
                state.set_connection_status(backend, ConnectionStatus::Disconnected);
                state.set_latency(backend, None);
                state.reset_history_fetches(backend);
            }
            BackendEvent::Error { message } => {
                state.apply(backend, server_info(format!("Error: {message}")));
                state.set_connection_status(backend, ConnectionStatus::Disconnected);
                state.set_latency(backend, None);
                state.reset_history_fetches(backend);
            }
            BackendEvent::Latency { ms } => {
                state.set_latency(backend, Some(ms));
                state.set_connection_status(backend, ConnectionStatus::Connected);
            }
            BackendEvent::HistoryFetched { target, at_start } => {
                state.finish_history_fetch(backend, target, at_start);
            }
            BackendEvent::Event(event) => {
                self.emit_lua_event(state, backend, &event);
                // Anchor the view: if the user has scrolled up in the focused
                // buffer, advance scroll_position so that the same messages
                // stay visible when a new one is appended at the tail. Skipped
                // while a history fetch is in flight: backfill inserts above
                // the viewport, and `scroll_position` counts from the newest
                // message, so head inserts leave it correct as-is - bumping
                // would jump the view toward older messages.
                let before = state.focused_buffer_mut(view).map(|b| {
                    (
                        b.messages.len(),
                        b.scroll_position,
                        b.history == HistoryState::Fetching,
                    )
                });
                state.apply(backend, event);
                if let Some((len_before, pos, fetching)) = before {
                    if pos > 0 && !fetching {
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
        // Silent state-only events have no Lua representation and no chat line.
        if event.is_silent_state_update() {
            return;
        }

        let Some(info) = state.backends.get(&backend).map(|b| b.info.clone()) else {
            return;
        };

        let target = event.target().cloned().unwrap_or_else(TargetId::status);
        let stored = StoredMessage::from_event(event.clone());

        let nickname = state.nickname(backend);
        let Ok(table) = to_lua_event(self.lua, &stored, &info, &target, target.as_str(), nickname)
        else {
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

/// Tracks the reaction pill, buffer-bar tab, and user-list row under the
/// cursor for hover highlighting. Returns `true` (triggering a repaint) when
/// any of the three changes; moves that touch none of them (or stay within
/// the same hit) do not repaint.
fn handle_mouse_moved(view: &mut ViewState, x: u16, y: u16) -> bool {
    let reaction_hit = view.layout.reaction_at(x, y).cloned();
    let reaction_changed = reaction_hit != view.hovered_reaction;
    if reaction_changed {
        view.hovered_reaction = reaction_hit;
    }

    let tab_hit = view.layout.tab_at(x, y).cloned();
    let tab_changed = tab_hit != view.hovered_tab;
    if tab_changed {
        view.hovered_tab = tab_hit;
    }

    let member_hit = view.layout.member_row_at(x, y);
    let member_changed = member_hit != view.hovered_member;
    if member_changed {
        view.hovered_member = member_hit;
    }

    reaction_changed || tab_changed || member_changed
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
fn close_buffer(state: &mut State, view: &mut ViewState, id: &tirc_core::BufferId) {
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
        time: None,
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
    use tirc_core::backend::BackendInfo;
    use tirc_core::{BufferId, MessageBody, MsgKind, Protocol, TargetId, UserRef};

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
    fn focus_clears_hovered_member_but_not_hovered_tab() {
        let (_, backend) = state_with_buffers(&["#a"]);
        let a = BufferId::new(backend, "#a");

        let mut view = ViewState::new();
        view.hovered_tab = Some(BarHit::Buffer(a.clone()));
        view.hovered_member = Some(2);

        view.focus(a.clone());

        assert_eq!(
            view.hovered_member, None,
            "a member index only names a row in the buffer it was hit-tested against"
        );
        assert_eq!(
            view.hovered_tab,
            Some(BarHit::Buffer(a)),
            "hovered_tab is keyed by a stable id, so it survives a focus change"
        );
    }

    #[test]
    fn handle_mouse_moved_reports_tab_hover_changes() {
        let tab_rect = ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: 5,
            height: 1,
        };
        let (_, backend) = state_with_buffers(&["#a"]);
        let hit = BarHit::Buffer(BufferId::new(backend, "#a"));

        let mut view = ViewState::new();
        view.layout.bar_tabs = vec![(tab_rect, hit.clone())];

        assert!(
            handle_mouse_moved(&mut view, 2, 0),
            "entering the tab's hit box changes hovered_tab"
        );
        assert_eq!(view.hovered_tab, Some(hit));
        assert_eq!(view.hovered_reaction, None);
        assert_eq!(view.hovered_member, None);

        assert!(
            !handle_mouse_moved(&mut view, 3, 0),
            "moving within the same tab does not change anything"
        );

        assert!(
            handle_mouse_moved(&mut view, 9, 0),
            "leaving the tab's hit box changes hovered_tab back to None"
        );
        assert_eq!(view.hovered_tab, None);
    }

    #[test]
    fn handle_mouse_moved_reports_member_hover_changes() {
        let mut view = ViewState::new();
        view.layout.userlist_rect = Some(ratatui::layout::Rect {
            x: 80,
            y: 0,
            width: 10,
            height: 5,
        });

        assert!(
            handle_mouse_moved(&mut view, 85, 1),
            "entering the user-list row changes hovered_member"
        );
        assert_eq!(view.hovered_member, Some(0));
        assert_eq!(view.hovered_tab, None);
        assert_eq!(view.hovered_reaction, None);

        assert!(
            !handle_mouse_moved(&mut view, 85, 1),
            "moving within the same row does not change anything"
        );

        assert!(
            handle_mouse_moved(&mut view, 85, 0),
            "moving onto the title row changes hovered_member back to None"
        );
        assert_eq!(view.hovered_member, None);
    }

    #[test]
    fn handle_mouse_moved_accumulates_independent_hover_changes() {
        // A move that changes both `hovered_tab` and `hovered_member` at once
        // (neither region overlaps the cursor any more) must repaint. A
        // short-circuiting implementation that stops after the first
        // unchanged kind - as the pre-hover-effects code did, when reactions
        // were the only kind tracked - would wrongly report `false` here.
        let (_, backend) = state_with_buffers(&["#a"]);
        let mut view = ViewState::new();
        view.hovered_tab = Some(BarHit::Buffer(BufferId::new(backend, "#a")));
        view.hovered_member = Some(3);
        view.hovered_reaction = None;

        assert!(
            handle_mouse_moved(&mut view, 200, 200),
            "moving off every hit-tested region still repaints"
        );
        assert_eq!(view.hovered_tab, None);
        assert_eq!(view.hovered_member, None);
        assert_eq!(view.hovered_reaction, None);
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

    #[test]
    fn away_text_sets_the_message() {
        assert_eq!(
            next_away_state(&None, "gone fishing"),
            Some("gone fishing".to_string())
        );
        // A new message replaces the current one without toggling back.
        assert_eq!(
            next_away_state(&Some("afk".to_string()), "lunch"),
            Some("lunch".to_string())
        );
    }

    #[test]
    fn away_no_arg_toggles() {
        assert_eq!(
            next_away_state(&None, ""),
            Some(DEFAULT_AWAY_MESSAGE.to_string())
        );
        assert_eq!(next_away_state(&Some("afk".to_string()), ""), None);
    }
}
