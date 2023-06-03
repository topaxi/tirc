use irc::proto::Message;
use tokio::sync::Mutex;

pub enum Mode {
    Normal,
    Insert,
}

pub struct State {
    pub mode: Mode,
    current_buffer: Mutex<String>,
    messages: Vec<Message>,
}

impl State {
    pub fn new() -> State {
        State {
            mode: Mode::Normal,
            current_buffer: Mutex::new(String::new()),
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
