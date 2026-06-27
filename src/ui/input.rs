use std::collections::HashMap;
use std::sync::Arc;

use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent};
use mlua::Lua;

use crate::backends::BackendHandle;
use crate::config::{emit_event, EventName};
use crate::core::{
    BackendEvent, BackendId, BackendMessage, ChatEvent, Command, MsgKind, TargetId, TxnAllocator,
};
use crate::tui::lua::{create_lua_sender, to_lua_event};
use crate::tui::Tui;

use super::state::StoredMessage;
use super::{Mode, State, ViewState};

/// Events the main loop feeds to the input handler.
#[derive(Debug)]
pub enum Event {
    Input(KeyEvent),
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
}

impl<'lua> InputHandler<'lua> {
    pub fn new(
        lua: &'lua Lua,
        ui: Tui,
        backends: Vec<BackendHandle>,
        txn: Arc<TxnAllocator>,
    ) -> Self {
        Self {
            lua,
            ui,
            backends,
            txn,
            senders: HashMap::new(),
        }
    }

    pub fn ui(&self) -> &Tui {
        &self.ui
    }

    pub fn render_ui(&mut self, state: &State, view: &ViewState) -> Result<(), anyhow::Error> {
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
            ["list"] => self.send_to(backend, Command::ListChannels),
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
        state.buffers.entry(buffer.clone()).or_default();
        view.focus(buffer);
        target
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
            Event::Input(key) => self.handle_key(state, view, key),
            Event::Backend(message) => {
                self.handle_backend(state, message);
                Ok(true)
            }
            Event::Tick => Ok(true),
        }
    }

    fn handle_key(
        &mut self,
        state: &mut State,
        view: &mut ViewState,
        event: KeyEvent,
    ) -> Result<bool, anyhow::Error> {
        match (view.mode, event.code) {
            (Mode::Normal, KeyCode::Tab) => view.next_buffer(state),
            (Mode::Normal, KeyCode::BackTab) => view.previous_buffer(state),
            (Mode::Normal, code) if Self::key_code_is_digit(code) => {
                let index = Self::get_key_code_as_digit(code) as usize;
                view.focus_buffer_index(state, index);
            }
            (Mode::Normal, KeyCode::Char('i')) => view.mode = Mode::Insert,
            (Mode::Normal, KeyCode::Char(':')) => view.mode = Mode::Command,
            (Mode::Command | Mode::Insert, KeyCode::Esc) => {
                view.mode = Mode::Normal;
                self.ui.reset_input();
            }
            (Mode::Command, KeyCode::Enter) => {
                let proceed = self.handle_command(state, view)?;
                self.ui.reset_input();
                return Ok(proceed);
            }
            (Mode::Insert, KeyCode::Enter) => {
                let message = self.ui.input().value().to_string();
                if !message.trim().is_empty() {
                    if let Some(buffer) = view.focused.clone() {
                        self.send(buffer.backend, buffer.target, message, MsgKind::Text);
                    }
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

    fn handle_backend(&mut self, state: &mut State, message: BackendMessage) {
        let backend = message.backend;

        match message.event {
            BackendEvent::Ready { nickname } => state.set_nickname(backend, nickname),
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
                state.apply(backend, event);
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

        let Ok(table) = to_lua_event(self.lua, &stored, &info, &target) else {
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

fn server_info(text: String) -> ChatEvent {
    ChatEvent::ServerInfo {
        target: None,
        from: None,
        code: None,
        text,
        raw: None,
    }
}
