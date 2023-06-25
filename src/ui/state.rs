use indexmap::IndexMap;

use irc::proto::{Command, Message};

#[derive(Clone, Copy, Debug)]
pub enum Mode {
    Normal,
    Command,
    Insert,
}

#[derive(Debug)]
pub struct State {
    pub mode: Mode,
    pub nickname: String,
    pub current_buffer: String,
    pub buffers: IndexMap<String, Vec<Message>>,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> State {
        let default_buffer_name = State::get_default_buffer_name();

        let buffers: IndexMap<String, Vec<Message>> = {
            let mut buffers = IndexMap::new();
            buffers.insert(default_buffer_name.clone(), vec![]);
            buffers
        };

        State {
            mode: Mode::Normal,
            nickname: String::new(),
            current_buffer: default_buffer_name,
            buffers,
        }
    }

    fn get_default_buffer_name() -> String {
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
            buffers.insert(buffer_name.to_string(), vec![]);
        }
    }

    pub fn push_message(&mut self, message: Message) {
        match &message.command {
            Command::PRIVMSG(nickname, _) | Command::NOTICE(nickname, _) => {
                let default_buffer_name = State::get_default_buffer_name();
                let mut target = match message.response_target() {
                    Some(response_target) if response_target != self.nickname => response_target,
                    _ => nickname,
                };

                if target == "*" || target == self.nickname {
                    target = default_buffer_name.as_str();
                }

                self.create_buffer_if_not_exists(target);
                self.buffers.get_mut(target).unwrap().push(message);
            }
            Command::TOPIC(channel, _)
            | Command::PART(channel, _)
            | Command::JOIN(channel, _, _) => {
                self.create_buffer_if_not_exists(channel);
                self.buffers.get_mut(channel).unwrap().push(message);
            }
            _ => {
                self.buffers
                    .get_mut(&State::get_default_buffer_name())
                    .unwrap()
                    .push(message);
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
        state.buffers.insert("foo".to_string(), vec![]);
        assert_eq!(state.get_current_buffer_index(), 0);
        state.set_current_buffer_index(1);
        assert_eq!(state.get_current_buffer_index(), 1);
    }

    #[test]
    fn test_set_current_buffer() {
        let mut state = super::State::default();
        state.buffers.insert("foo".to_string(), vec![]);
        assert_eq!(state.get_current_buffer_index(), 0);
        state.set_current_buffer("foo");
        assert_eq!(state.get_current_buffer_index(), 1);
    }

    #[test]
    fn test_next_buffer() {
        let mut state = super::State::default();
        state.buffers.insert("foo".to_string(), vec![]);
        state.buffers.insert("bar".to_string(), vec![]);
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
        state.buffers.insert("foo".to_string(), vec![]);
        state.buffers.insert("bar".to_string(), vec![]);
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
