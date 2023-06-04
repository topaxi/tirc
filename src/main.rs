extern crate irc;

use std::time::Duration;

use crossterm::event::{self, Event as CrosstermEvent, KeyCode, KeyEvent};
use futures::prelude::*;
use irc::client::{prelude::*, ClientStream};

use tirc::ui;
use tokio::{sync::mpsc, time::Instant};
use tui_input::backend::crossterm::EventHandler;

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
    let mut ui = tirc::tui::Ui::new()?;

    ui.initialize_terminal()?;

    loop {
        ui.render(&state)?;

        match rx.recv().await {
            Some(Event::Input(event)) => match state.mode {
                ui::Mode::Normal => match event.code {
                    KeyCode::Char('i') => {
                        state.mode = ui::Mode::Insert;
                    }
                    KeyCode::Char(':') => {
                        state.mode = ui::Mode::Command;
                    }
                    _ => {}
                },
                ui::Mode::Command | ui::Mode::Insert => match event.code {
                    KeyCode::Esc => {
                        state.mode = ui::Mode::Normal;

                        ui.input.reset();
                    }
                    KeyCode::Enter => {
                        match state.mode {
                            ui::Mode::Command => {
                                state.mode = ui::Mode::Normal;

                                let value = ui.input.value();

                                match value {
                                    "q" | "quit" => {
                                        irc.send_quit("tirc")?;
                                        input_handle.abort();
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                            ui::Mode::Insert => {
                                let message = ui.input.value();

                                irc.send_privmsg("#test", message)?;
                            }
                            _ => {}
                        }

                        ui.input.reset();
                    }
                    _ => {
                        ui.input.handle_event(&CrosstermEvent::Key(event));
                    }
                },
            },
            Some(Event::Message(message)) => {
                state.push_message(message);
            }
            Some(Event::Tick) | None => {}
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

#[derive(Debug)]
enum Event<I> {
    Input(I),
    Message(Message),
    Tick,
}

async fn poll_input(tx: mpsc::Sender<Event<KeyEvent>>) -> Result<(), failure::Error> {
    let tick_rate = Duration::from_millis(200);
    let mut last_tick = Instant::now();

    loop {
        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if event::poll(timeout).expect("poll works") {
            if let CrosstermEvent::Key(key) = event::read().expect("can read events") {
                tx.send(Event::Input(key)).await.expect("can send events");
            }
        }

        if last_tick.elapsed() >= tick_rate {
            if let Ok(_) = tx.send(Event::Tick).await {
                last_tick = Instant::now();
            }
        }
    }
}

async fn create_irc_client() -> Result<Client, failure::Error> {
    let config = Config {
        nickname: Some(format!("topaxci")),
        server: Some(format!("irc.topaxi.ch")),
        port: Some(6697),
        use_tls: Some(true),
        dangerously_accept_invalid_certs: Some(true),
        channels: [format!("#test")].to_vec(),
        version: Some(format!("tirc v0.1.0 - https://github.com/topaxi/tirc")),
        ..Default::default()
    };

    let client = Client::from_config(config).await?;

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
