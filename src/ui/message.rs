use mlua::Lua;

use crate::tui::lua::to_lua_message;

#[derive(Debug)]
pub enum TircMessage {
    Irc(
        Box<chrono::DateTime<chrono::Local>>,
        Box<irc::proto::Message>,
        Box<mlua::Table>,
    ),
    Lua(Box<chrono::DateTime<chrono::Local>>, Box<mlua::Table>),
}

impl TircMessage {
    pub fn from_message(message: Box<irc::proto::Message>, lua: &Lua) -> mlua::Result<Self> {
        let lua_message = to_lua_message(lua, &message)?.into();

        Ok(TircMessage::Irc(
            chrono::Local::now().into(),
            message,
            lua_message,
        ))
    }

    pub fn get_lua_message(&self) -> &mlua::Table {
        match self {
            TircMessage::Irc(_, _, lua_message) => lua_message,
            TircMessage::Lua(_, lua_message) => lua_message,
        }
    }
}
