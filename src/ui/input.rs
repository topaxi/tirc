use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
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

    pub fn sync_state(&mut self, state: &mut State) -> Result<(), failure::Error> {
        state.nickname = self.irc.current_nickname().to_string();

        let channels = match self.irc.list_channels() {
            Some(channels) => channels,
            None => vec![],
        };

        let buffers = &mut state.buffers;

        for channel in channels {
            if buffers.get(&channel).is_none() {
                buffers.insert(channel, vec![]);
            }
        }

        Ok(())
    }

    pub fn render_ui(&mut self, state: &State) -> Result<(), failure::Error> {
        self.ui.render(&self.irc, &state)?;

        Ok(())
    }

    fn handle_command(&mut self, state: &mut State) -> Result<(), anyhow::Error> {
        state.mode = Mode::Normal;

        let command: Vec<&str> = self.ui.input().value().splitn(2, ' ').collect();

        match command[..] {
            ["m" | "msg", target_and_message] => {
                match target_and_message.splitn(2, ' ').collect::<Vec<&str>>()[..] {
                    [target, message] => {
                        state.create_buffer_if_not_exists(&target);
                        state.set_current_buffer(&target);
                        self.irc.send_privmsg(&target, &message)?;
                    }
                    _ => {}
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

        Ok(())
    }

    pub fn handle_event(
        &mut self,
        state: &mut State,
        event: Event<crossterm::event::KeyEvent>,
    ) -> Result<(), anyhow::Error> {
        match (state.mode, event) {
            (_, Event::Input(event)) if event == KeyEvent::from(KeyCode::Tab) => {
                state.next_buffer();
            }
            (_, Event::Input(event))
                if event == KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT) =>
            {
                state.previous_buffer();
            }
            (Mode::Normal, Event::Input(event)) => match event.code {
                KeyCode::Char('i') => {
                    state.mode = Mode::Insert;
                }
                KeyCode::Char(':') => {
                    state.mode = Mode::Command;
                }
                _ => {}
            },
            (Mode::Command | Mode::Insert, Event::Input(event)) => match event.code {
                KeyCode::Esc => {
                    state.mode = Mode::Normal;

                    self.ui.reset_input();
                }
                KeyCode::Enter => {
                    match state.mode {
                        Mode::Command => {
                            self.handle_command(state)?;
                        }
                        Mode::Insert => {
                            let message = self.ui.input().value();
                            let current_buffer = &state.current_buffer;

                            self.irc.send_privmsg(current_buffer, message)?;
                        }
                        _ => {}
                    }

                    self.ui.reset_input();
                }
                _ => {
                    self.ui.handle_event(&CrosstermEvent::Key(event));
                }
            },
            (_, Event::Message(message)) => {
                state.push_message(message);
            }
            (_, Event::Tick) => {}
        }

        Ok(())
    }
}
