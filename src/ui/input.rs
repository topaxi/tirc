use crossterm::event::{Event as CrosstermEvent, KeyCode};
use irc::{client::prelude::Client, proto::Message};
use mlua::Lua;

use crate::tui::Tui;

use super::{Mode, State};

#[derive(Debug)]
pub enum Event<I> {
    Input(I),
    Message(Box<Message>),
    Tick,
}

pub struct InputHandler {
    lua: Lua,
    irc: Client,
    ui: Tui,
}

impl InputHandler {
    pub fn new(lua: Lua, irc: Client, ui: Tui) -> Self {
        Self { lua, irc, ui }
    }

    pub fn ui(&self) -> &Tui {
        &self.ui
    }

    pub fn sync_state(&mut self, state: &mut State) -> Result<(), anyhow::Error> {
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

    pub fn render_ui(&mut self, state: &State) -> Result<(), anyhow::Error> {
        self.ui.render(&self.irc, &self.lua, state)?;

        Ok(())
    }

    fn handle_command(&mut self, state: &mut State) -> Result<(), anyhow::Error> {
        state.mode = Mode::Normal;

        let command: Vec<&str> = self.ui.input().value().splitn(2, ' ').collect();

        match command[..] {
            ["m" | "msg", target_and_message] => {
                match target_and_message.splitn(2, ' ').collect::<Vec<&str>>()[..] {
                    [target, message] => {
                        state.create_buffer_if_not_exists(target);
                        state.set_current_buffer(target);

                        if !message.trim().is_empty() {
                            self.irc.send_privmsg(target, message)?;
                        }
                    }
                    [target] => {
                        state.create_buffer_if_not_exists(target);
                        state.set_current_buffer(target);
                    }
                    _ => {}
                }
            }
            ["notice", target_and_message] => {
                if let [target, message] =
                    target_and_message.splitn(2, ' ').collect::<Vec<&str>>()[..]
                {
                    state.create_buffer_if_not_exists(target);
                    self.irc.send_notice(target, message)?;
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

    fn key_code_is_digit(key_code: KeyCode) -> bool {
        match key_code {
            KeyCode::Char(char) => char.is_ascii_digit(),
            _ => false,
        }
    }

    fn get_key_code_as_digit(key_code: KeyCode) -> u8 {
        match key_code {
            KeyCode::Char(char) => char.to_digit(10).unwrap() as u8,
            _ => 0,
        }
    }

    pub fn handle_event(
        &mut self,
        state: &mut State,
        event: Event<crossterm::event::KeyEvent>,
    ) -> Result<(), anyhow::Error> {
        match (state.mode, event) {
            (_, Event::Input(event)) if event.code == KeyCode::Tab => {
                state.next_buffer();
            }
            (_, Event::Input(event)) if event.code == KeyCode::BackTab => {
                state.previous_buffer();
            }
            (_, Event::Input(event)) if InputHandler::key_code_is_digit(event.code) => {
                let index = InputHandler::get_key_code_as_digit(event.code) as usize;

                if index < state.buffers.len() {
                    state.set_current_buffer_index(index);
                }
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
                state.push_message(*message);
            }
            (_, Event::Tick) => {}
        }

        Ok(())
    }
}
