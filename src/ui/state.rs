use tokio::sync::Mutex;

pub struct State {
    current_buffer: Mutex<String>,
}
