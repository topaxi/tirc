extern crate irc;

use std::{rc::Rc, time::Duration};

use crossterm::event::{self, Event as CrosstermEvent, KeyEvent};
use futures::prelude::*;
use irc::client::{prelude::*, ClientStream};

use tirc::{
    config::{load_config, TircConfig},
    ui::{self, Event, InputHandler},
};
use tokio::{sync::mpsc, time::Instant};

fn create_lua_irc_sender(
    lua: &mlua::Lua,
    sender: irc::client::Sender,
) -> mlua::Result<mlua::Table> {
    let shared_sender = Rc::new(sender);
    let tbl = lua.create_table()?;

    let sender = Rc::clone(&shared_sender);
    tbl.set(
        "send_privmsg",
        lua.create_function(move |_, (target, message): (String, String)| {
            sender.send_privmsg(target, message).unwrap();
            Ok(())
        })?,
    )?;

    let sender = Rc::clone(&shared_sender);
    tbl.set(
        "send_notice",
        lua.create_function(move |_, (target, message): (String, String)| {
            sender.send_notice(target, message).unwrap();
            Ok(())
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

async fn root_task(
    rt: &tokio::runtime::Runtime,
    lua: &mlua::Lua,
    config: &TircConfig,
) -> Result<(), anyhow::Error> {
    let (irc, stream) = setup_irc(config, lua).await?;

    let (tx, mut rx) = mpsc::channel(16);

    let input_sender = tx.clone();
    let irc_sender = tx.clone();

    let input_handle = rt.spawn(async move { poll_input(input_sender).await });
    let irc_handle = rt.spawn(async move { connect_irc(stream, irc_sender).await });

    let mut state = ui::State {
        server: config.servers.first().unwrap().host.clone(),
        ..Default::default()
    };

    let mut tui = tirc::tui::Tui::new()?;

    tui.initialize_terminal()?;

    let mut input_handler = InputHandler::new(lua, irc, tui);

    loop {
        input_handler.sync_state(&mut state)?;
        input_handler.render_ui(&state)?;

        if let Some(event) = rx.recv().await {
            if let Err(err) = input_handler.handle_event(&mut state, event) {
                eprintln!("Error: {:?}", err);
                input_handle.abort();
                irc_handle.abort();
                break;
            }
        }
    }

    let res = tokio::try_join!(input_handle, irc_handle);

    Ok(match res {
        Ok((_, _)) => Ok(()),
        Err(e) => {
            if e.is_cancelled() {
                Ok(())
            } else {
                Err(e)
            }
        }
    }?)
}

fn main() -> Result<(), anyhow::Error> {
    let lua = mlua::Lua::new();
    let config = load_config(&lua)?;

    let threads = usize::min(2, num_cpus::get());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()?;

    rt.block_on(root_task(&rt, &lua, &config))
}

async fn poll_input(tx: mpsc::Sender<Event<KeyEvent>>) -> Result<(), anyhow::Error> {
    let tick_rate = Duration::from_millis(1000);
    let mut last_tick = Instant::now();

    loop {
        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if event::poll(timeout)? {
            if let CrosstermEvent::Key(key) = event::read()? {
                tx.send(Event::Input(key)).await?;
            }
        }

        if last_tick.elapsed() >= tick_rate {
            last_tick = Instant::now();
            tx.send(Event::Tick).await?;
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn create_irc_client(config: &TircConfig) -> Result<Client, anyhow::Error> {
    let server_config = config.servers.first().expect("No server config found");

    let client_config = Config {
        nickname: Some(
            server_config
                .nickname
                .first()
                .expect("No nickname found")
                .clone(),
        ),
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

async fn connect_irc(
    mut stream: ClientStream,
    tx: mpsc::Sender<Event<KeyEvent>>,
) -> Result<(), anyhow::Error> {
    while let Some(message) = stream.next().await.transpose()? {
        tx.send(Event::Message(Box::new(message))).await?;
    }

    Ok(())
}
