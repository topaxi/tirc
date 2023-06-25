extern crate irc;

use std::time::Duration;

use crossterm::event::{self, Event as CrosstermEvent, KeyEvent};
use futures::prelude::*;
use irc::client::{prelude::*, ClientStream};

use tirc::{
    config::{load_config, TircConfig},
    ui::{self, Event, InputHandler},
};
use tokio::{sync::mpsc, time::Instant};

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let (config, lua) = load_config().await?;

    let mut irc = create_irc_client(&config).await?;
    let stream = irc.stream()?;

    irc.send_cap_req(&[
        Capability::EchoMessage,
        Capability::MultiPrefix,
        Capability::ExtendedJoin,
        Capability::AwayNotify,
        Capability::ChgHost,
        Capability::AccountNotify,
        Capability::ServerTime,
        Capability::UserhostInNames,
    ])?;

    irc.identify()?;

    let (tx, mut rx) = mpsc::channel(16);

    let input_sender = tx.clone();
    let irc_sender = tx.clone();

    let input_handle = tokio::spawn(async move { poll_input(input_sender).await });

    let irc_handle = tokio::spawn(async move { connect_irc(stream, irc_sender).await });

    let mut state = ui::State {
        server: config.servers.get(0).unwrap().host.clone(),
        ..Default::default()
    };

    let mut tui = tirc::tui::Tui::new()?;

    tui.initialize_terminal()?;

    let mut input_handler = InputHandler::new(lua, irc, tui);

    loop {
        input_handler.sync_state(&mut state)?;
        input_handler.render_ui(&state)?;

        if let Some(event) = rx.recv().await {
            match input_handler.handle_event(&mut state, event) {
                Ok(_) => {}
                Err(_) => {
                    input_handle.abort();
                    break;
                }
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

async fn poll_input(tx: mpsc::Sender<Event<KeyEvent>>) -> Result<(), failure::Error> {
    let tick_rate = Duration::from_millis(200);
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

        if last_tick.elapsed() < tick_rate {
            continue;
        }

        if (tx.send(Event::Tick).await).is_ok() {
            last_tick = Instant::now();
        }
    }
}

async fn create_irc_client(config: &TircConfig) -> Result<Client, anyhow::Error> {
    let server_config = config.servers.get(0).expect("No server config found");

    let client_config = Config {
        nickname: Some(
            server_config
                .nickname
                .get(0)
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
) -> Result<(), failure::Error> {
    while let Some(message) = stream.next().await.transpose()? {
        tx.send(Event::Message(Box::new(message))).await?;
    }

    Ok(())
}
