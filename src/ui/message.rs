use mlua::Lua;

use crate::tui::lua::to_lua_message;

#[derive(Debug)]
pub enum TircMessage<'lua> {
    Irc(
        Box<chrono::DateTime<chrono::Local>>,
        Box<irc::proto::Message>,
        Box<mlua::Table<'lua>>,
    ),
    Lua(Box<chrono::DateTime<chrono::Local>>, Box<mlua::Table<'lua>>),
}

impl<'lua> TircMessage<'lua> {
    pub fn from_message(message: Box<irc::proto::Message>, lua: &'lua Lua) -> Self {
        let lua_message = to_lua_message(lua, &message).unwrap().into();

        TircMessage::Irc(chrono::Local::now().into(), message, lua_message)
    }

    pub fn get_lua_message(&self) -> &mlua::Table<'lua> {
        match self {
            TircMessage::Irc(_, _, lua_message) => lua_message,
            TircMessage::Lua(_, lua_message) => lua_message,
        }
    }
}
