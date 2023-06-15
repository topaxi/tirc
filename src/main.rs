extern crate irc;

use std::path::Path;
use std::time::Duration;

use crossterm::event::{self, Event as CrosstermEvent, KeyEvent};
use futures::prelude::*;
use indoc::indoc;
use irc::client::{prelude::*, ClientStream};

use mlua::{Lua, LuaSerdeExt, Table};
use serde::Deserialize;
use tirc::ui::{self, Event, InputHandler};
use tokio::{sync::mpsc, time::Instant};

#[tokio::main]
async fn main() -> Result<(), failure::Error> {
    let mut irc = create_irc_client().await?;
    let stream = irc.stream()?;

    irc.identify()?;

    let (tx, mut rx) = mpsc::channel(16);

    let input_sender = tx.clone();
    let irc_sender = tx.clone();

    let input_handle = tokio::spawn(async move { poll_input(input_sender).await });

    let irc_handle = tokio::spawn(async move { connect_irc(stream, irc_sender).await });

    let mut state = ui::State::new();
    let mut tui = tirc::tui::Tui::new()?;

    tui.initialize_terminal()?;

    let mut input_handler = InputHandler::new(irc, tui);

    loop {
        input_handler.sync_state(&mut state)?;
        input_handler.render_ui(&state)?;

        match rx.recv().await {
            Some(event) => match input_handler.handle_event(&mut state, event) {
                Ok(_) => {}
                Err(_) => {
                    input_handle.abort();
                    break;
                }
            },
            None => {}
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

        if last_tick.elapsed() >= tick_rate {
            if let Ok(_) = tx.send(Event::Tick).await {
                last_tick = Instant::now();
            }
        }
    }
}

#[inline]
fn bool_true() -> bool {
    true
}

#[inline]
fn default_port() -> u16 {
    6697
}

#[derive(Deserialize, Debug)]
struct ServerConfig {
    host: String,

    #[serde(default = "default_port")]
    port: u16,

    #[serde(default = "bool_true")]
    use_tls: bool,

    #[serde(default)]
    accept_invalid_cert: bool,

    nickname: Vec<String>,

    #[serde(default)]
    autojoin: Vec<String>,
}

#[derive(Deserialize, Debug)]
struct TircConfig {
    servers: Vec<ServerConfig>,
}

fn get_default_config() -> &'static str {
    let default_config = indoc! {"
        local config = {}

        config.servers = {
          {
            host = 'irc.topaxi.ch',
            nickname = { 'Rincewind', 'Twoflower' },
            port = 6697,
            use_tls = true,
            autojoin = { '#tirc' },
          },
        }

        return config
    "};

    default_config
}

async fn load_config() -> Result<TircConfig, failure::Error> {
    let config_filename =
        xdg::BaseDirectories::with_prefix("tirc")?.place_config_file("init.lua")?;
    let config_dirname = config_filename
        .parent()
        .expect("Unable to create config directory");

    if !config_filename.exists() {
        std::fs::create_dir_all(&config_dirname)?;

        std::fs::write(&config_filename, get_default_config())?;
    }

    let config_lua_code = std::fs::read_to_string(&config_filename)?;

    let lua = Lua::new();

    let globals = lua.globals();

    let package: Table = globals.get("package")?;
    let package_path: String = package.get("path")?;
    let mut path_array: Vec<String> = package_path.split(";").map(|s| s.to_owned()).collect();

    fn prefix_path(array: &mut Vec<String>, path: &Path) {
        array.insert(0, format!("{}/?.lua", path.display()));
        array.insert(1, format!("{}/?/init.lua", path.display()));
    }

    prefix_path(&mut path_array, config_dirname);

    package.set("path", path_array.join(";"))?;

    let value = lua
        .load(&config_lua_code)
        .set_name(config_filename.display().to_string())?
        .call(())?;
    let config: TircConfig = lua.from_value(value)?;

    Ok(config)
}

async fn create_irc_client() -> Result<Client, failure::Error> {
    let config = load_config().await?;

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
        server: Some(server_config.host.clone()),
        port: Some(server_config.port),
        use_tls: Some(server_config.use_tls),
        dangerously_accept_invalid_certs: Some(server_config.accept_invalid_cert),
        channels: server_config.autojoin.clone(),
        version: Some(format!("tirc v0.1.0 - https://github.com/topaxi/tirc")),
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
        tx.send(Event::Message(message)).await?;
    }

    Ok(())
}
