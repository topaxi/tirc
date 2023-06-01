extern crate irc;

use std::time::Duration;

use crossterm::{
    event::{self, Event as CrosstermEvent, KeyCode, KeyEvent},
    terminal::{disable_raw_mode, enable_raw_mode},
};
use futures::prelude::*;
use irc::client::prelude::*;

use tokio::{
    sync::mpsc::{self, Sender},
    time::Instant,
};
use tui::{
    style::{Color, Style},
    widgets::{Block, Borders, List, ListItem},
};

#[tokio::main]
async fn main() -> Result<(), failure::Error> {
    enable_raw_mode()?;

    let mut terminal = tirc::tui::Terminal::new()?;

    terminal.clear()?;

    let (tx, mut rx) = mpsc::channel(16);

    let input_sender = tx.clone();
    let irc_sender = tx.clone();

    let input_handle = tokio::spawn(async move { handle_input(input_sender).await });

    let irc_handle = tokio::spawn(async move { connect_irc(irc_sender).await });

    let mut messages: Vec<Message> = Vec::new();

    loop {
        match rx.recv().await {
            Some(Event::Input(event)) => {
                println!("{:?}", event);

                match event.code {
                    KeyCode::Char('q') => {
                        // TODO: Broadcast quit to irc
                        break;
                    }
                    _ => {}
                }
            }
            Some(Event::Message(message)) => {
                messages.push(message);
            }
            Some(Event::Tick) => {}
            None => {}
        }

        render_ui(&messages)?;
    }

    disable_raw_mode()?;

    let res = tokio::try_join!(input_handle, irc_handle);

    Ok(match res {
        Ok((_, _)) => Ok(()),
        Err(e) => Err(e),
    }?)
}

#[derive(Debug)]
enum Event<I> {
    Input(I),
    Message(Message),
    Tick,
}

async fn handle_input(tx: Sender<Event<KeyEvent>>) -> Result<(), failure::Error> {
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

fn render_ui(messages: &Vec<Message>) -> Result<(), failure::Error> {
    let mut terminal = tirc::tui::Terminal::new()?;

    terminal.draw(|f| {
        let size = f.size();
        let messages: Vec<_> = messages
            .iter()
            .rev()
            .take(size.height as usize)
            .map(|message| {
                ListItem::new(message.to_string()).style(Style::default().fg(Color::White))
            })
            .rev()
            .collect();

        let list = List::new(messages).block(
            Block::default()
                .title("irc.topaxi.ch")
                .borders(Borders::NONE),
        );

        f.render_widget(list, size);
    })?;

    Ok(())
}

async fn connect_irc(tx: Sender<Event<KeyEvent>>) -> Result<(), failure::Error> {
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
        tx.send(Event::Message(message)).await?;
    }

    Ok(())
}
