extern crate irc;

use std::{sync::Arc, time::Duration};

use crossterm::event::{Event as CrosstermEvent, EventStream};
use futures::prelude::*;
use futures::stream::FusedStream;
use irc::client::{prelude::*, ClientStream};

use tirc::{
    config::{load_config, TircConfig},
    ui::{self, Event, InputHandler},
};

const TICK_RATE: Duration = Duration::from_millis(1000);

fn create_lua_irc_sender(
    lua: &mlua::Lua,
    sender: irc::client::Sender,
) -> mlua::Result<mlua::Table> {
    let shared_sender = Arc::new(sender);
    let tbl = lua.create_table()?;

    let sender = Arc::clone(&shared_sender);
    tbl.set(
        "send_privmsg",
        lua.create_function(move |_, (target, message): (String, String)| {
            sender
                .send_privmsg(target, message)
                .map_err(mlua::Error::external)
        })?,
    )?;

    let sender = Arc::clone(&shared_sender);
    tbl.set(
        "send_notice",
        lua.create_function(move |_, (target, message): (String, String)| {
            sender
                .send_notice(target, message)
                .map_err(mlua::Error::external)
        })?,
    )?;

    Ok(tbl)
}

async fn setup_irc(
    config: &TircConfig,
    lua: &mlua::Lua,
) -> Result<(Client, ClientStream), anyhow::Error> {
    let mut irc = create_irc_client(config).await?;
    let stream = irc.stream()?;

    lua.set_named_registry_value("sender", create_lua_irc_sender(lua, irc.sender())?)?;

    irc.send_cap_req(&[
        Capability::EchoMessage,
        Capability::MultiPrefix,
        Capability::ExtendedJoin,
        Capability::AwayNotify,
        Capability::ChgHost,
        Capability::AccountNotify,
        Capability::ServerTime,
        Capability::UserhostInNames,
        Capability::Batch,
        Capability::Custom("labeled-response"),
    ])?;

    irc.identify()?;

    Ok((irc, stream))
}

async fn root_task(lua: &mlua::Lua, config: &TircConfig) -> Result<(), anyhow::Error> {
    let (irc, irc_stream) = setup_irc(config, lua).await?;
    let mut irc_stream = irc_stream.fuse();

    let mut state = ui::State {
        server: config
            .servers
            .first()
            .ok_or_else(|| anyhow::anyhow!("No server configured in init.lua"))?
            .host
            .clone(),
        ..Default::default()
    };

    let mut tui = tirc::tui::Tui::new()?;

    tui.initialize_terminal()?;

    let mut input_handler = InputHandler::new(lua, irc, tui);

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(TICK_RATE);

    let terminate = terminate_signal();
    tokio::pin!(terminate);

    loop {
        input_handler.sync_state(&mut state)?;
        input_handler.render_ui(&state)?;

        let event = tokio::select! {
            Some(event) = events.next() => match event? {
                CrosstermEvent::Key(key) => Event::Input(key),
                _ => continue,
            },
            message = irc_stream.next(), if !irc_stream.is_terminated() => {
                match message.transpose()? {
                    Some(message) => Event::Message(Box::new(message)),
                    None => continue,
                }
            }
            _ = tick.tick() => Event::Tick,
            _ = &mut terminate => break,
        };

        if let Err(err) = input_handler.handle_event(&mut state, event) {
            eprintln!("Error: {:?}", err);
            break;
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
        let mut hangup =
            signal(SignalKind::hangup()).expect("failed to install SIGHUP handler");

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

    tirc::tui::Tui::install_panic_hook();

    let threads = usize::min(2, num_cpus::get());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()?;

    rt.block_on(root_task(&lua, &config))
}

async fn create_irc_client(config: &TircConfig) -> Result<Client, anyhow::Error> {
    let server_config = config
        .servers
        .first()
        .ok_or_else(|| anyhow::anyhow!("No server configured in init.lua (servers is empty)"))?;

    let nickname = server_config.nickname.first().cloned().ok_or_else(|| {
        anyhow::anyhow!(
            "Server '{}' has an empty nickname list in init.lua",
            server_config.host
        )
    })?;

    let client_config = Config {
        nickname: Some(nickname),
        alt_nicks: server_config.nickname[1..].to_vec(),
        realname: server_config.realname.clone(),
        server: Some(server_config.host.clone()),
        port: Some(server_config.port),
        use_tls: Some(server_config.use_tls),
        dangerously_accept_invalid_certs: Some(server_config.accept_invalid_cert),
        channels: server_config.autojoin.clone(),
        version: Some(format!(
            "tirc v{} - https://github.com/topaxi/tirc",
            env!("CARGO_PKG_VERSION")
        )),
        ..Default::default()
    };

    let client = Client::from_config(client_config).await?;

    Ok(client)
}
