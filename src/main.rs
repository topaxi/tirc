extern crate irc;

use std::{process, time::Duration};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent},
    terminal::{disable_raw_mode, enable_raw_mode},
};
use futures::prelude::*;
use irc::client::prelude::*;

use tokio::{
    sync::mpsc::{self, Sender},
    time::Instant,
};
use tui::widgets::{Block, Borders};

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let (itx, mut irx) = mpsc::channel(10);

    let input_handle = tokio::spawn(async move { handle_input(itx).await });

    let ui_handle = tokio::spawn(async move { render_ui().await });

    let irc_handle = tokio::spawn(async move { connect_irc().await });

    match irx.recv().await {
        Some(InputEvent::Input(event)) => match event.code {
            KeyCode::Char('q') => {
                disable_raw_mode()?;
            }
            _ => {}
        },
        Some(InputEvent::Tick) => {}
        None => {}
    }

    let res = tokio::try_join!(input_handle, ui_handle, irc_handle);

    Ok(match res {
        Ok((_, _, _)) => Ok(()),
        Err(e) => Err(e),
    }?)
}

#[derive(Debug)]
enum InputEvent<I> {
    Input(I),
    Tick,
}

async fn handle_input(tx: Sender<InputEvent<KeyEvent>>) -> Result<(), failure::Error> {
    let tick_rate = Duration::from_millis(200);
    let mut last_tick = Instant::now();

    loop {
        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if event::poll(timeout).expect("poll works") {
            if let Event::Key(key) = event::read().expect("can read events") {
                tx.send(InputEvent::Input(key))
                    .await
                    .expect("can send events");
            }
        }

        if last_tick.elapsed() >= tick_rate {
            if let Ok(_) = tx.send(InputEvent::Tick).await {
                last_tick = Instant::now();
            }
        }
    }
}

async fn render_ui() -> Result<(), failure::Error> {
    enable_raw_mode()?;

    let mut terminal = tirc::tui::Terminal::new()?;

    terminal.draw(|f| {
        let size = f.size();
        let block = Block::default().title("Block").borders(Borders::NONE);

        f.render_widget(block, size);
    })?;

    Ok(())
}

async fn connect_irc() -> Result<(), failure::Error> {
    let config = Config {
        nickname: Some(format!("topaxci")),
        server: Some(format!("irc.topaxi.ch")),
        port: Some(6697),
        use_tls: Some(true),
        dangerously_accept_invalid_certs: Some(true),
        ..Default::default()
    };

    let mut client = Client::from_config(config).await?;

    client.identify()?;

    let mut stream = client.stream()?;

    while let Some(message) = stream.next().await.transpose()? {
        print!("{}", message);
    }

    Ok(())
}
