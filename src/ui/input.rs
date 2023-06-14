use crossterm::event::{Event as CrosstermEvent, KeyCode};
use irc::{client::prelude::Client, proto::Message};

use crate::tui::Tui;

use super::{Mode, State};

#[derive(Debug)]
pub enum Event<I> {
    Input(I),
    Message(Message),
    Tick,
}

pub struct InputHandler {
    irc: Client,
    ui: Tui,
}

impl InputHandler {
    pub fn new(irc: Client, ui: Tui) -> Self {
        Self { irc, ui }
    }

    pub fn ui(&self) -> &Tui {
        &self.ui
    }

    pub fn render_ui(&mut self, state: &State) -> Result<(), failure::Error> {
        self.ui.render(&self.irc, &state)?;

        Ok(())
    }

    pub fn handle_event(
        &mut self,
        state: &mut State,
        event: Event<crossterm::event::KeyEvent>,
    ) -> Result<(), anyhow::Error> {
        match event {
            Event::Input(event) => match state.mode {
                Mode::Normal => match event.code {
                    KeyCode::Char('i') => {
                        state.mode = Mode::Insert;
                    }
                    KeyCode::Char(':') => {
                        state.mode = Mode::Command;
                    }
                    _ => {}
                },
                Mode::Command | Mode::Insert => match event.code {
                    KeyCode::Esc => {
                        state.mode = Mode::Normal;

                        self.ui.reset_input();
                    }
                    KeyCode::Enter => {
                        match state.mode {
                            Mode::Command => {
                                state.mode = Mode::Normal;

                                let command: Vec<&str> =
                                    self.ui.input().value().splitn(2, ' ').collect();

                                match command[..] {
                                    ["m" | "msg", target_and_message] => {
                                        let target_and_message: Vec<&str> =
                                            target_and_message.splitn(2, ' ').collect();

                                        if target_and_message.len() == 2 {
                                            self.irc.send_privmsg(
                                                target_and_message[0],
                                                target_and_message[1],
                                            )?;
                                        }
                                    }
                                    ["q" | "quit"] => {
                                        self.irc.send_quit("tirc")?;
                                        return Err(anyhow::Error::msg("quit"));
                                    }
                                    ["j" | "join", channel] => {
                                        self.irc.send_join(channel)?;
                                    }
                                    ["p" | "part", channel] => {
                                        self.irc.send_part(channel)?;
                                    }
                                    _ => {}
                                }
                            }
                            Mode::Insert => {
                                let message = self.ui.input().value();

                                self.irc.send_privmsg("#test", message)?;
                            }
                            _ => {}
                        }

                        self.ui.reset_input();
                    }
                    _ => {
                        self.ui.handle_event(&CrosstermEvent::Key(event));
                    }
                },
            },
            Event::Message(message) => {
                state.push_message(message);
            }
            Event::Tick => {}
        }

        Ok(())
    }
}
