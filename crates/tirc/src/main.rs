use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event as CrosstermEvent, EventStream};
use futures::StreamExt;

use tirc_core::ChatEvent;

use anyhow::Context;

use tirc_backend_irc::{IrcBackend, IrcBackendConfig};
use tirc_backend_matrix::{MatrixBackend, MatrixBackendConfig};
use tirc_backend_mattermost::{MattermostBackend, MattermostBackendConfig};
use tirc_config::{load_config, ServerConfig, TircConfig};
use tirc_core::backend::{spawn as spawn_backend, ChatBackend};
use tirc_core::{BackendId, BackendMessage, BufferId, Protocol, TxnAllocator};
use tirc_tui::preview::{build_client, link_preview_worker};
use tirc_tui::{
    DecodeRequest, DecodedImage, EncodedImage, PreviewCacheStore, PreviewRequest, PreviewResult, Tui,
};
use tirc_ui::{State, ViewState};

use crate::input::{Event, InputHandler};

mod input;

use ratatui::layout::Size;
use ratatui_image::{
    picker::{Picker, ProtocolType},
    protocol::{iterm2::Iterm2, sixel::Sixel},
    Resize,
};

const TICK_RATE: Duration = Duration::from_millis(1000);

/// Force a repaint after this many idle ticks even if nothing marked the frame
/// dirty. A safety net so any state change that forgets to set `dirty` self-heals
/// within a few seconds; the counter resets on every real render, so active use
/// never triggers a redundant heartbeat repaint.
const RENDER_HEARTBEAT_TICKS: u32 = 5;

/// Builds a backend from one server config entry, dispatching on its `protocol`
/// and validating that the required fields for that protocol are present.
fn build_backend(id: BackendId, server: &ServerConfig) -> anyhow::Result<Box<dyn ChatBackend>> {
    match server.protocol {
        Protocol::Irc => {
            let host = server
                .host
                .clone()
                .context("IRC server entry is missing `host`")?;

            if server.nickname.is_empty() {
                anyhow::bail!("IRC server '{host}' has an empty `nickname` list");
            }

            Ok(Box::new(IrcBackend::new(
                id,
                IrcBackendConfig {
                    host,
                    port: server.port,
                    use_tls: server.use_tls,
                    accept_invalid_cert: server.accept_invalid_cert,
                    nickname: server.nickname.clone(),
                    realname: server.realname.clone(),
                    autojoin: server.autojoin.clone(),
                },
            )))
        }
        Protocol::Matrix => {
            let homeserver = server
                .homeserver
                .clone()
                .context("Matrix server entry is missing `homeserver`")?;
            let user_id = server
                .user_id
                .clone()
                .with_context(|| format!("Matrix server '{homeserver}' is missing `user_id`"))?;
            let password = server
                .password
                .clone()
                .with_context(|| format!("Matrix server '{homeserver}' is missing `password`"))?;

            Ok(Box::new(MatrixBackend::new(
                id,
                MatrixBackendConfig {
                    homeserver,
                    user_id,
                    password,
                    device_id: server.device_id.clone(),
                    autojoin: server.autojoin.clone(),
                    store_dir: None,
                },
            )))
        }
        Protocol::Mattermost => {
            let url = server
                .url
                .clone()
                .context("Mattermost server entry is missing `url`")?;
            let team = server
                .team
                .clone()
                .with_context(|| format!("Mattermost server '{url}' is missing `team`"))?;

            if server.token.is_none() && (server.user_id.is_none() || server.password.is_none()) {
                anyhow::bail!(
                    "Mattermost server '{url}' needs either `token` or both `user_id` and `password`"
                );
            }

            Ok(Box::new(MattermostBackend::new(
                id,
                MattermostBackendConfig {
                    url,
                    token: server.token.clone(),
                    login_id: server.user_id.clone(),
                    password: server.password.clone(),
                    team,
                    autojoin: server.autojoin.clone(),
                },
            )))
        }
    }
}

async fn root_task(
    lua: &mlua::Lua,
    config: &TircConfig,
    config_path: &std::path::Path,
) -> Result<(), anyhow::Error> {
    if config.servers.is_empty() {
        anyhow::bail!("No server configured in init.lua (servers is empty)");
    }

    let txn = Arc::new(TxnAllocator::new());
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<BackendMessage>();

    let mut state = State::new();
    let mut view = ViewState::new();
    let mut handles = Vec::new();

    let alias_store = tirc_config::aliases::AliasStore::load();
    let order_store = tirc_config::buffer_order::BufferOrderStore::load();
    let ui_prefs = tirc_config::ui_prefs::UiPrefsStore::load();
    view.buffer_bar_style = ui_prefs.buffer_bar().map(str::to_string);

    // Config buffer ranks count globally across servers so servers keep their
    // config order relative to each other.
    let mut config_rank = 0;
    let mut backend_names: HashMap<String, BackendId> = HashMap::new();

    for (index, server) in config.servers.iter().enumerate() {
        if !server.enabled {
            continue;
        }
        let id = BackendId(index);
        tirc_lua::runtime::register_backend_metadata(lua, id)?;
        let backend = build_backend(id, server)?;
        let info = backend.info();
        for (target, name) in &server.aliases {
            state
                .config_aliases
                .insert(BufferId::new(id, target.as_str()), name.clone());
        }
        for (target, name) in alias_store.aliases_for(&info.name) {
            state
                .user_aliases
                .insert(BufferId::new(id, target), name.to_string());
        }
        for target in &server.buffer_order {
            state
                .config_order
                .insert(BufferId::new(id, target.as_str()), config_rank);
            config_rank += 1;
        }
        backend_names.insert(info.name.clone(), id);
        state.register_backend(info);
        view.focus_if_unset(BufferId::status(id));
        handles.push(spawn_backend(backend, event_tx.clone()));
    }
    drop(event_tx);

    // The persisted `:bufmove` order is a flat cross-server list, so it can
    // only be resolved once every backend's name is known.
    for (rank, (server, target)) in order_store.iter().enumerate() {
        if let Some(&id) = backend_names.get(server) {
            state.user_order.insert(BufferId::new(id, target), rank);
        }
    }
    state.sort_buffers();

    let mut tui = Tui::new()?;
    let picker = tui.initialize_terminal(config.image_protocol)?;
    tui.set_quick_reactions(&config.quick_reactions);

    // Inline images are decoded and encoded off the main loop: the renderer sends
    // decode requests over `decode_tx`, a background worker turns them into encoded
    // protocols, and the results come back over `decoded_rx` to be cached. Both
    // channels stay unused (the worker is never spawned) when graphics are
    // unavailable, so media falls back to its textual line.
    let (decode_tx, decode_rx) = tokio::sync::mpsc::unbounded_channel::<DecodeRequest>();
    let (decoded_tx, mut decoded_rx) = tokio::sync::mpsc::unbounded_channel::<DecodedImage>();
    // In tmux, the cursor-positioned protocols (iTerm2/Sixel) are encoded
    // unwrapped and positioned absolutely at draw time, so images land in this
    // pane even while another pane is active. Kitty stays on the widget path:
    // its unicode placeholders are position-safe under tmux.
    let tmux_abs_position = tirc_tui::tmux::in_tmux()
        && picker.as_ref().is_some_and(|picker| {
            matches!(
                picker.protocol_type(),
                ProtocolType::Iterm2 | ProtocolType::Sixel
            )
        });
    if let Some(picker) = picker {
        tui.set_decode_sender(decode_tx);
        tokio::spawn(image_decode_worker(
            picker,
            tmux_abs_position,
            decode_rx,
            decoded_tx,
        ));
    }
    if tmux_abs_position {
        tui.refresh_pane_origin();
    }

    // Link previews follow the same off-loop model as images: the renderer sends
    // a fetch request per URL it draws, a background worker fetches/parses the
    // Open Graph metadata, and results come back over `preview_result_rx` to be
    // cached in the renderer. Gated by config; both channels stay unused when
    // disabled so no URLs are ever contacted.
    let (preview_tx, preview_rx) = tokio::sync::mpsc::unbounded_channel::<PreviewRequest>();
    let (preview_result_tx, mut preview_result_rx) =
        tokio::sync::mpsc::unbounded_channel::<PreviewResult>();
    if config.link_previews {
        let cache_dir = xdg::BaseDirectories::with_prefix("tirc")
            .create_cache_directory("previews")
            .unwrap_or_else(|_| std::env::temp_dir().join("tirc-previews"));
        tui.set_preview_store(PreviewCacheStore::load(cache_dir.clone()));
        tui.set_preview_sender(preview_tx);
        tokio::spawn(link_preview_worker(
            build_client(),
            cache_dir,
            preview_rx,
            preview_result_tx,
        ));
    }

    // Host tasks Lua submits from `tirc.spawn`/`tirc.fetch`: completions come
    // back over this channel so their callbacks run on this (the Lua) thread.
    let (host_task_tx, mut host_task_rx) =
        tokio::sync::mpsc::unbounded_channel::<tirc_lua::host_tasks::HostTaskMessage>();
    lua.set_app_data(tirc_lua::host_tasks::HostTaskSender(host_task_tx));

    let mut input_handler = InputHandler::new(
        lua,
        tui,
        handles,
        txn,
        config_path.to_owned(),
        config.auto_reload_config,
        config.watch_files.clone(),
        config.selection_mode,
        config.quick_reactions.clone(),
        alias_store,
        order_store,
        ui_prefs,
    );

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(TICK_RATE);

    let terminate = terminate_signal();
    tokio::pin!(terminate);

    let mut idle_ticks: u32 = 0;

    loop {
        // Render only when something changed; `dirty` starts set so the first
        // frame always paints, and idle ticks / mouse moves no longer repaint.
        if input_handler.take_dirty() {
            input_handler.render_ui(&state, &mut view)?;
            idle_ticks = 0;
        }

        let event = tokio::select! {
            Some(event) = events.next() => match event {
                Ok(CrosstermEvent::Key(key)) => Event::Input(key),
                Ok(CrosstermEvent::Mouse(mouse)) => Event::Mouse(mouse),
                Ok(CrosstermEvent::Paste(text)) => Event::Paste(text),
                Ok(CrosstermEvent::Resize(_, _)) => {
                    if tmux_abs_position {
                        input_handler.refresh_pane_origin();
                    }
                    input_handler.mark_dirty();
                    continue;
                }
                Ok(CrosstermEvent::FocusGained) => {
                    input_handler.set_terminal_focus(true);
                    if tmux_abs_position {
                        input_handler.refresh_pane_origin();
                        // tmux may have repainted the window while we were
                        // unfocused (e.g. after a window switch), wiping the
                        // passthrough pixels; unchanged cells would never
                        // re-emit them, so force a full repaint.
                        if input_handler.has_cached_images() {
                            input_handler.force_redraw();
                        }
                    }
                    continue;
                }
                Ok(CrosstermEvent::FocusLost) => {
                    input_handler.set_terminal_focus(false);
                    if tmux_abs_position {
                        input_handler.refresh_pane_origin();
                    }
                    continue;
                }
                Err(_) => continue,
            },
            Some(message) = event_rx.recv() => Event::Backend(message),
            Some(decoded) = decoded_rx.recv() => {
                input_handler.insert_decoded_image(decoded);
                input_handler.mark_dirty();
                continue;
            }
            Some(result) = preview_result_rx.recv() => {
                input_handler.insert_link_preview(result);
                input_handler.mark_dirty();
                continue;
            }
            Some(message) = host_task_rx.recv() => {
                input_handler.on_host_task(&mut state, &mut view, message);
                continue;
            }
            _ = tick.tick() => {
                idle_ticks += 1;
                if idle_ticks >= RENDER_HEARTBEAT_TICKS {
                    input_handler.mark_dirty();
                }
                // Keep the pane origin fresh so layout changes that fire no
                // event here (e.g. swap-pane) correct themselves within a
                // tick. Only worth a subprocess while images are on screen.
                if tmux_abs_position && input_handler.has_cached_images() {
                    input_handler.refresh_pane_origin();
                }
                // Debounced persistence of link previews fetched since the last
                // tick, so a burst of results is written once rather than per URL.
                input_handler.flush_preview_cache();
                Event::Tick
            }
            _ = &mut terminate => break,
        };

        match input_handler.handle_event(&mut state, &mut view, event) {
            Ok(true) => {}
            Ok(false) => break,
            Err(err) => {
                // Surface handler errors to the focused buffer's status rather
                // than exiting, so a transient Lua or IRC error is recoverable.
                input_handler.mark_dirty();
                if let Some(backend) = view.focused.as_ref().map(|b| b.backend) {
                    state.apply(
                        backend,
                        ChatEvent::ServerInfo {
                            target: None,
                            from: None,
                            code: None,
                            text: format!("Error: {err}"),
                            raw: None,
                            time: None,
                        },
                    );
                }
            }
        }
    }

    Ok(())
}

/// Background image-decode worker. Owns the terminal graphics [`Picker`] and turns
/// decode requests into encoded protocols off the main loop: each request is
/// decoded and encoded on the blocking pool, so several images in a freshly-opened
/// buffer decode concurrently, and the result is sent back for the main loop to
/// cache. The `Picker` is cheap to clone and its `new_protocol` takes `&self`, so
/// cloning it per job is safe.
async fn image_decode_worker(
    picker: Picker,
    tmux_abs_position: bool,
    mut requests: tokio::sync::mpsc::UnboundedReceiver<DecodeRequest>,
    decoded: tokio::sync::mpsc::UnboundedSender<DecodedImage>,
) {
    while let Some(request) = requests.recv().await {
        let picker = picker.clone();
        let decoded = decoded.clone();
        tokio::task::spawn_blocking(move || {
            let protocol = decode_image(&picker, tmux_abs_position, &request.path, request.avail);
            let _ = decoded.send(DecodedImage {
                path: request.path,
                protocol,
            });
        });
    }
}

/// Opens, decodes, and encodes one image fitted to `avail`. Returns `None` on any
/// failure so the caller records the path as unrenderable rather than retrying it.
///
/// With `tmux_abs_position`, the protocol is encoded *without* ratatui-image's
/// passthrough wrapping (and with its escapes pre-doubled): the renderer wraps
/// it per frame in a passthrough that positions the outer terminal's cursor at
/// the pane-absolute cell, which is what keeps images inside this pane while
/// another tmux pane is active.
fn decode_image(
    picker: &Picker,
    tmux_abs_position: bool,
    path: &std::path::Path,
    avail: Size,
) -> Option<EncodedImage> {
    let image = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    if !tmux_abs_position {
        return picker
            .new_protocol(image, avail, Resize::Fit(None))
            .ok()
            .map(EncodedImage::Widget);
    }
    let resize = Resize::Fit(None);
    let size = resize.size_for(&image, picker.font_size(), avail);
    let resized = resize.resize(&image, picker.font_size(), size, None);
    let data = match picker.protocol_type() {
        ProtocolType::Sixel => Sixel::new(resized, size, false).ok()?.data,
        _ => Iterm2::new(resized, size, false).ok()?.data,
    };
    Some(EncodedImage::TmuxRaw {
        data_doubled: data.replace('\x1b', "\x1b\x1b"),
        size,
    })
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
    // Install log capture first so config loading and everything after is
    // recorded for the `:debug` pane.
    tirc_core::logging::init();

    let lua = mlua::Lua::new();
    let (config, config_path) = load_config(&lua)?;

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
    let result = local.block_on(&runtime, root_task(&lua, &config, &config_path));

    // Let the terminal restore (Tui::drop) before surfacing any error.
    drop(local);
    result
}
