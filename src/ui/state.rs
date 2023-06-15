use indexmap::IndexMap;

use irc::proto::Message;

#[derive(Clone, Copy, Debug)]
pub enum Mode {
    Normal,
    Command,
    Insert,
}

#[derive(Debug)]
pub struct State {
    pub mode: Mode,
    pub current_buffer: String,
    pub buffers: IndexMap<String, Vec<Message>>,
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
            current_buffer: default_buffer_name,
            buffers,
            messages: Vec::new(),
        }
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
        match message.response_target() {
            Some(target) => {
                self.create_buffer_if_not_exists(target);
                let buffers = &mut self.buffers;
                let buffer_messages = buffers.get_mut(target).unwrap();

                buffer_messages.push(message);
            }
            None => self.messages.push(message),
        }
    }

    pub fn get_messages(&self) -> &Vec<Message> {
        &self.messages
    }
}
