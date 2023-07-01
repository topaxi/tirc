#[derive(Debug)]
pub enum TircMessage<'lua> {
    Irc(
        Box<chrono::DateTime<chrono::Local>>,
        Box<irc::proto::Message>,
        Box<Option<mlua::Value<'lua>>>,
    ),
    Lua(Box<chrono::DateTime<chrono::Local>>, Box<mlua::Value<'lua>>),
}
