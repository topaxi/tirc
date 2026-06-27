//! Protocol backends and the contract between them and the core.
//!
//! A backend owns one network connection. It runs as its own task on the tokio
//! runtime (so it may be `Send` and use `Send` futures, e.g. `matrix-sdk`),
//! emitting normalized [`BackendMessage`]s onto a shared channel and consuming
//! [`Command`]s addressed to it. The `!Send` Lua/UI loop never touches a backend
//! directly; it only drains events and enqueues commands through the channels
//! here.

use tokio::sync::mpsc;

use crate::core::{BackendEvent, BackendId, BackendMessage, Command, Protocol};

pub mod irc;
pub mod matrix;

/// Sender half of the shared channel every backend emits onto. Unbounded so a
/// backend never blocks on a slow UI, and so the UI can always make progress
/// draining it.
pub type EventSender = mpsc::UnboundedSender<BackendMessage>;

/// Receiver half of a single backend's command queue. Unbounded so the Lua
/// command-enqueue bridge can push synchronously without awaiting.
pub type CommandReceiver = mpsc::UnboundedReceiver<Command>;

/// Identifying metadata about a running backend, kept by the core for display
/// (buffer/window titles) and protocol-aware routing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendInfo {
    pub id: BackendId,
    pub protocol: Protocol,
    /// Human-facing name, e.g. the IRC server host or Matrix homeserver.
    pub name: String,
}

/// One connected network. Implementors translate their wire protocol into
/// [`BackendMessage`]s and apply incoming [`Command`]s.
#[async_trait::async_trait]
pub trait ChatBackend: Send + 'static {
    fn info(&self) -> BackendInfo;

    /// Drive the connection until it closes or the command channel is dropped.
    /// Returning `Err` is surfaced to the core as a [`BackendEvent::Error`].
    async fn run(
        self: Box<Self>,
        events: EventSender,
        commands: CommandReceiver,
    ) -> anyhow::Result<()>;
}

/// The core's handle to a spawned backend: send it commands and read its info.
#[derive(Debug)]
pub struct BackendHandle {
    info: BackendInfo,
    commands: mpsc::UnboundedSender<Command>,
}

impl BackendHandle {
    pub fn info(&self) -> &BackendInfo {
        &self.info
    }

    pub fn id(&self) -> BackendId {
        self.info.id
    }

    /// Enqueue a command for the backend. Synchronous and non-blocking, so it is
    /// safe to call from the Lua callback path. Fails only if the backend task
    /// has stopped.
    pub fn send(&self, command: Command) -> anyhow::Result<()> {
        self.commands
            .send(command)
            .map_err(|_| anyhow::anyhow!("backend {:?} is no longer running", self.info.id))
    }

    /// A clone of the command sender, for building Lua closures that enqueue
    /// commands directly (the `event` callback's sender bridge).
    pub fn sender(&self) -> mpsc::UnboundedSender<Command> {
        self.commands.clone()
    }
}

/// Spawn a backend onto the current runtime, wiring its command queue and the
/// shared event channel. Returns the handle the core keeps; the backend object
/// itself lives only inside the spawned task.
pub fn spawn(backend: Box<dyn ChatBackend>, events: EventSender) -> BackendHandle {
    let info = backend.info();
    let id = info.id;
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        let result = backend.run(events.clone(), command_rx).await;

        let event = match result {
            Ok(()) => BackendEvent::Disconnected { reason: None },
            Err(err) => BackendEvent::Error {
                message: err.to_string(),
            },
        };

        let _ = events.send(BackendMessage { backend: id, event });
    });

    BackendHandle {
        info,
        commands: command_tx,
    }
}
