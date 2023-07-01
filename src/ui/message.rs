#[derive(Debug)]
pub enum TircMessage<'lua> {
    Irc(
        Box<chrono::DateTime<chrono::Local>>,
        Box<irc::proto::Message>,
        Box<Option<mlua::Value<'lua>>>,
    ),
    Lua(
        Box<chrono::DateTime<chrono::Local>>,
        Box<String>,
        Box<mlua::Value<'lua>>,
    ),
}

impl<'lua> From<irc::proto::Message> for TircMessage<'lua> {
    fn from(value: irc::proto::Message) -> Self {
        TircMessage::Irc(chrono::Local::now().into(), value.into(), None.into())
    }
}
