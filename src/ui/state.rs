use std::rc::Rc;

use indexmap::IndexMap;

use irc::{
    client::data::User,
    proto::{Command, Message},
};

use super::message::TircMessage;

#[derive(Clone, Copy, Debug)]
pub enum Mode {
    Normal,
    Command,
    Insert,
}

#[derive(Debug, Default)]
pub struct ChatBuffer<'lua> {
    pub messages: Vec<TircMessage<'lua>>,
    pub scroll_position: usize,
}

#[derive(Debug)]
pub struct State<'lua> {
    pub ui_layout_rects: Rc<[tui::layout::Rect]>,
    pub mode: Mode,
    pub nickname: String,
    pub server: String,
    pub current_buffer: String,
    pub buffers: IndexMap<String, ChatBuffer<'lua>>,
    pub users_in_current_buffer: Rc<[User]>,
}

impl<'lua> Default for State<'lua> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'lua> State<'lua> {
    pub fn new() -> State<'lua> {
        let default_buffer_name = State::get_default_buffer_name();

        let buffers = {
            let mut buffers = IndexMap::new();
            buffers.insert(default_buffer_name.to_string(), ChatBuffer::default());
            buffers
        };

        State {
            ui_layout_rects: Rc::new([]),
            mode: Mode::Normal,
            nickname: String::new(),
            server: String::new(),
            current_buffer: default_buffer_name,
            buffers,
            users_in_current_buffer: Rc::new([]),
        }
    }

    pub fn get_default_buffer_name() -> String {
        String::from("(status)")
    }

    fn get_buffer_name_by_index(&self, index: usize) -> String {
        let buffers = &self.buffers;
        let buffer_name = buffers.keys().nth(index).unwrap();
        buffer_name.to_string()
    }

    fn get_current_buffer_index(&self) -> usize {
        let buffers = &self.buffers;
        let current_buffer_name = &self.current_buffer;

        buffers
            .keys()
            .position(|name| name == current_buffer_name)
            .unwrap()
    }

    pub fn get_current_buffer(&mut self) -> Option<&mut ChatBuffer<'lua>> {
        self.buffers.get_mut(&self.current_buffer)
    }

    pub fn next_buffer(&mut self) {
        let buffers = &self.buffers;
        let current_buffer_index = self.get_current_buffer_index();
        let next_buffer_index = (current_buffer_index + 1) % buffers.len();
        self.current_buffer = self.get_buffer_name_by_index(next_buffer_index);
    }

    pub fn previous_buffer(&mut self) {
        let buffers = &self.buffers;
        let current_buffer_index = self.get_current_buffer_index();
        let previous_buffer_index = (current_buffer_index + buffers.len() - 1) % buffers.len();
        self.current_buffer = self.get_buffer_name_by_index(previous_buffer_index);
    }

    pub fn set_current_buffer_index(&mut self, index: usize) {
        self.current_buffer = self.get_buffer_name_by_index(index);
    }

    pub fn set_current_buffer(&mut self, buffer_name: &str) {
        self.current_buffer = buffer_name.to_string();
    }

    pub fn create_buffer_if_not_exists(&mut self, buffer_name: &str) {
        let buffers = &mut self.buffers;

        if buffers.get(buffer_name).is_none() {
            buffers.insert(buffer_name.to_string(), ChatBuffer::default());
        }
    }

    fn push_message_to_buffer(&mut self, buffer_name: &str, message: TircMessage<'lua>) {
        let buffer = self.buffers.get_mut(buffer_name).unwrap();

        if let TircMessage::Irc(_, m, _) = &message {
            if let Some(tags) = &m.tags {
                if let Some(label) = tags.iter().find(|tag| tag.0 == "label") {
                    // Find index of message with same tag label
                    let index = buffer.messages.iter().position(|m| match m {
                        TircMessage::Irc(_, m, _) => m.tags.as_ref().map_or(false, |tags| {
                            tags.iter().any(|tag| tag.0 == "label" && tag.1 == label.1)
                        }),
                        _ => false,
                    });

                    if let Some(index) = index {
                        // Remove old message
                        buffer.messages[index] = message;

                        return;
                    }
                }
            }
        }

        buffer.messages.push(message);
    }

    fn get_target_buffer_name(&mut self, message: &Message) -> String {
        let default_buffer_name = State::get_default_buffer_name();

        match &message.command {
            Command::PRIVMSG(nickname, _) | Command::NOTICE(nickname, _) => {
                let mut target = match message.response_target() {
                    Some(response_target) if response_target != self.nickname => {
                        response_target.to_owned()
                    }
                    _ => nickname.to_owned(),
                };

                if target == "*" || target == self.nickname {
                    target = default_buffer_name;
                }

                target
            }
            Command::TOPIC(channel, _)
            | Command::ChannelMODE(channel, _)
            | Command::PART(channel, _)
            | Command::JOIN(channel, _, _) => channel.to_owned(),
            _ => default_buffer_name,
        }
    }

    pub fn push_message(&mut self, message: TircMessage<'lua>) {
        let buffer_name = match &message {
            TircMessage::Irc(_, m, _) => self.get_target_buffer_name(m),
            _ => State::get_default_buffer_name(),
        };

        self.create_buffer_if_not_exists(&buffer_name);
        self.push_message_to_buffer(&buffer_name, message)
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::state::ChatBuffer;

    #[test]
    fn test_get_buffer_name_by_index() {
        let state = super::State::default();
        assert_eq!(state.get_buffer_name_by_index(0), "(status)");
    }

    #[test]
    fn test_get_current_buffer_index() {
        let state = super::State::default();
        assert_eq!(state.get_current_buffer_index(), 0);
    }

    #[test]
    fn test_set_current_buffer_index() {
        let mut state = super::State::default();
        state
            .buffers
            .insert("foo".to_string(), ChatBuffer::default());
        assert_eq!(state.get_current_buffer_index(), 0);
        state.set_current_buffer_index(1);
        assert_eq!(state.get_current_buffer_index(), 1);
    }

    #[test]
    fn test_set_current_buffer() {
        let mut state = super::State::default();
        state
            .buffers
            .insert("foo".to_string(), ChatBuffer::default());
        assert_eq!(state.get_current_buffer_index(), 0);
        state.set_current_buffer("foo");
        assert_eq!(state.get_current_buffer_index(), 1);
    }

    #[test]
    fn test_next_buffer() {
        let mut state = super::State::default();
        state
            .buffers
            .insert("foo".to_string(), ChatBuffer::default());
        state
            .buffers
            .insert("bar".to_string(), ChatBuffer::default());
        assert_eq!(state.get_current_buffer_index(), 0);
        state.next_buffer();
        assert_eq!(state.get_current_buffer_index(), 1);
        state.next_buffer();
        assert_eq!(state.get_current_buffer_index(), 2);
        state.next_buffer();
        assert_eq!(state.get_current_buffer_index(), 0);
    }

    #[test]
    fn test_previous_buffer() {
        let mut state = super::State::default();
        state
            .buffers
            .insert("foo".to_string(), ChatBuffer::default());
        state
            .buffers
            .insert("bar".to_string(), ChatBuffer::default());
        assert_eq!(state.get_current_buffer_index(), 0);
        state.previous_buffer();
        assert_eq!(state.get_current_buffer_index(), 2);
        state.previous_buffer();
        assert_eq!(state.get_current_buffer_index(), 1);
        state.previous_buffer();
        assert_eq!(state.get_current_buffer_index(), 0);
    }

    #[test]
    fn test_create_buffer_if_not_exists() {
        let mut state = super::State::default();
        state.create_buffer_if_not_exists("foo");
        assert_eq!(state.buffers.len(), 2);
        state.create_buffer_if_not_exists("foo");
        assert_eq!(state.buffers.len(), 2);
    }
}
