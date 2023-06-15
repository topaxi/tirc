use indexmap::IndexMap;

use irc::proto::Message;
use std::sync::Mutex;

pub enum Mode {
    Normal,
    Command,
    Insert,
}

pub struct State {
    pub mode: Mode,
    pub current_buffer: Mutex<String>,
    pub buffers: Mutex<IndexMap<String, Vec<Message>>>,
    messages: Vec<Message>,
}

impl State {
    pub fn new() -> State {
        let default_buffer_name = String::from("(status)");

        let buffers: IndexMap<String, Vec<Message>> = {
            let mut buffers = IndexMap::new();
            buffers.insert(default_buffer_name.clone(), vec![]);
            buffers
        };

        State {
            mode: Mode::Normal,
            current_buffer: Mutex::new(default_buffer_name),
            buffers: Mutex::new(buffers),
            messages: Vec::new(),
        }
    }

    pub fn push_message(&mut self, message: Message) {
        self.messages.push(message);
    }

    pub fn get_messages(&self) -> &Vec<Message> {
        &self.messages
    }
}
