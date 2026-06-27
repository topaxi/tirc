use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event as CrosstermEvent, EventStream};
use futures::StreamExt;

use tirc::backends::irc::{IrcBackend, IrcBackendConfig};
use tirc::backends::{self, ChatBackend};
use tirc::config::{load_config, ServerConfig, TircConfig};
use tirc::core::{BackendId, BackendMessage, BufferId, TxnAllocator};
use tirc::tui::Tui;
use tirc::ui::{Event, InputHandler, State, ViewState};

const TICK_RATE: Duration = Duration::from_millis(1000);

/// Builds a backend from one server config entry. All servers are IRC in Phase
/// 1; Phase 2 dispatches on a protocol tag.
fn build_backend(id: BackendId, server: &ServerConfig) -> anyhow::Result<Box<dyn ChatBackend>> {
    Ok(Box::new(IrcBackend::new(
        id,
        IrcBackendConfig {
            host: server.host.clone(),
            port: server.port,
            use_tls: server.use_tls,
            accept_invalid_cert: server.accept_invalid_cert,
            nickname: server.nickname.clone(),
            realname: server.realname.clone(),
            autojoin: server.autojoin.clone(),
        },
    )))
}

async fn root_task(lua: &mlua::Lua, config: &TircConfig) -> Result<(), anyhow::Error> {
    if config.servers.is_empty() {
        anyhow::bail!("No server configured in init.lua (servers is empty)");
    }

    let txn = Arc::new(TxnAllocator::new());
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<BackendMessage>();

    let mut state = State::new();
    let mut view = ViewState::new();
    let mut handles = Vec::new();

    for (index, server) in config.servers.iter().enumerate() {
        let id = BackendId(index);
        let backend = build_backend(id, server)?;
        state.register_backend(backend.info());
        view.focus_if_unset(BufferId::status(id));
        handles.push(backends::spawn(backend, event_tx.clone()));
    }
    drop(event_tx);

    let mut tui = Tui::new()?;
    tui.initialize_terminal()?;

    let mut input_handler = InputHandler::new(lua, tui, handles, txn);

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(TICK_RATE);

    let terminate = terminate_signal();
    tokio::pin!(terminate);

    loop {
        input_handler.render_ui(&state, &view)?;

        let event = tokio::select! {
            Some(event) = events.next() => match event? {
                CrosstermEvent::Key(key) => Event::Input(key),
                _ => continue,
            },
            Some(message) = event_rx.recv() => Event::Backend(message),
            _ = tick.tick() => Event::Tick,
            _ = &mut terminate => break,
        };

        match input_handler.handle_event(&mut state, &mut view, event) {
            Ok(true) => {}
            Ok(false) => break,
            Err(err) => return Err(err),
        }
    }

    Ok(())
}

/// Resolves when the process receives a termination signal, so the main loop
/// can break and let `Tui::drop` restore the terminal instead of being killed
/// mid-render.
async fn terminate_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut terminate =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        let mut interrupt =
            signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
        let mut hangup = signal(SignalKind::hangup()).expect("failed to install SIGHUP handler");

        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
            _ = hangup.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn main() -> Result<(), anyhow::Error> {
    let lua = mlua::Lua::new();
    let config = load_config(&lua)?;

    Tui::install_panic_hook();

    // A multi-thread runtime hosts the (Send) backend tasks; the !Send Lua/UI
    // loop is pinned to one thread via a LocalSet so mlua needs no `send`
    // feature.
    let threads = usize::min(
        2,
        std::thread::available_parallelism().map_or(1, |n| n.get()),
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()?;

    let local = tokio::task::LocalSet::new();
    let result = local.block_on(&runtime, root_task(&lua, &config));

    // Let the terminal restore (Tui::drop) before surfacing any error.
    drop(local);
    result
}
