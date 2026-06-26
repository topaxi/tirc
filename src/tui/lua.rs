use std::str::FromStr;

use irc::client::data::User;
use irc::proto::{Command, Message, Prefix};
use mlua::LuaSerdeExt;
use ratatui::style::Color;

use crate::lua::get_or_create_module;

fn get_tirc_theme_module(lua: &mlua::Lua) -> mlua::Table {
    get_or_create_module(lua, "tirc.tui.theme").expect("Unable to create tirc.tui.theme module")
}

pub fn create_tirc_theme_lua_module(lua: &mlua::Lua) -> mlua::Result<mlua::Table> {
    let module = get_tirc_theme_module(lua);

    module.set(
        "color",
        lua.create_function(|lua, (r, g, b): (u8, u8, u8)| lua.to_value(&Color::Rgb(r, g, b)))?,
    )?;

    module.set(
        "color_from_str",
        lua.create_function(|lua, str: String| lua.to_value(&Color::from_str(&str).unwrap()))?,
    )?;

    module.set(
        "style",
        lua.create_function(|lua, tbl: mlua::Table| {
            let mut style = ratatui::style::Style::default();

            if let Ok(Some(color)) = tbl.get::<Option<String>>("fg") {
                style = style.fg(Color::from_str(&color).unwrap());
            }

            if let Ok(Some(color)) = tbl.get::<Option<String>>("bg") {
                style = style.bg(Color::from_str(&color).unwrap());
            }

            lua.to_value(&style)
        })?,
    )?;

    Ok(module)
}

/// Splits the raw parameter portion of an IRC command into individual
/// parameters, treating a leading `:` as the start of a single trailing
/// parameter that runs to the end of the line.
fn split_params(rest: &str) -> Vec<String> {
    let mut params = Vec::new();
    let mut remaining = rest;

    loop {
        remaining = remaining.trim_start_matches(' ');

        if remaining.is_empty() {
            break;
        }

        if let Some(trailing) = remaining.strip_prefix(':') {
            params.push(trailing.to_string());
            break;
        }

        match remaining.find(' ') {
            Some(idx) => {
                params.push(remaining[..idx].to_string());
                remaining = &remaining[idx + 1..];
            }
            None => {
                params.push(remaining.to_string());
                break;
            }
        }
    }

    params
}

/// Builds a Lua representation of an IRC message.
///
/// The resulting table has a flat, plugin-friendly shape:
///
/// ```lua
/// {
///   command = 'PRIVMSG',          -- verb, or symbolic name for numeric replies
///   params = { '#channel', 'hi' },
///   nick = 'alice',               -- nil unless the prefix carries a nickname
///   user = '~alice',
///   host = 'example.com',
///   server = 'irc.example.com',   -- set instead of nick/user/host for server prefixes
///   tags = { { 'time', '...' } },
/// }
/// ```
///
/// `tostring(message)` yields the raw IRC line.
pub fn to_lua_message(lua: &mlua::Lua, message: &Message) -> mlua::Result<mlua::Table> {
    let table = lua.create_table()?;

    let raw_command = String::from(&message.command);
    let (verb, rest) = match raw_command.split_once(' ') {
        Some((verb, rest)) => (verb.to_string(), rest),
        None => (raw_command.clone(), ""),
    };

    // Numeric replies expose their symbolic name (e.g. `RPL_WELCOME`) instead of
    // the bare numeric code, which makes themes far easier to read.
    let command = match &message.command {
        Command::Response(response, _) => format!("{:?}", response),
        _ => verb,
    };

    table.set("command", command)?;

    let params = lua.create_table()?;
    for (index, param) in split_params(rest).into_iter().enumerate() {
        params.set(index + 1, param)?;
    }
    table.set("params", params)?;

    match &message.prefix {
        Some(Prefix::Nickname(nick, user, host)) => {
            table.set("nick", nick.as_str())?;
            table.set("user", user.as_str())?;
            table.set("host", host.as_str())?;
        }
        Some(Prefix::ServerName(server)) => {
            table.set("server", server.as_str())?;
        }
        None => {}
    }

    let tags = lua.create_table()?;
    if let Some(message_tags) = &message.tags {
        for (index, tag) in message_tags.iter().enumerate() {
            let lua_tag = lua.create_table()?;
            lua_tag.set(1, tag.0.as_str())?;
            lua_tag.set(2, tag.1.clone())?;
            tags.set(index + 1, lua_tag)?;
        }
    }
    table.set("tags", tags)?;

    table.set("raw", message.to_string().trim_end().to_string())?;

    let metatable = lua.create_table()?;
    metatable.set(
        "__tostring",
        lua.create_function(|_, table: mlua::Table| table.get::<String>("raw"))?,
    )?;
    table.set_metatable(Some(metatable))?;

    Ok(table)
}

/// Builds a Lua representation of a channel user.
///
/// ```lua
/// {
///   nickname = 'alice',
///   access_levels = { 'Voice' },
///   highest_access_level = 'Voice',
/// }
/// ```
pub fn to_lua_user(lua: &mlua::Lua, user: &User) -> mlua::Result<mlua::Table> {
    let table = lua.create_table()?;

    table.set("nickname", user.get_nickname())?;

    let access_levels = lua.create_table()?;
    for (index, access_level) in user.access_levels().into_iter().enumerate() {
        access_levels.set(index + 1, format!("{:?}", access_level))?;
    }
    table.set("access_levels", access_levels)?;

    table.set(
        "highest_access_level",
        format!("{:?}", user.highest_access_level()),
    )?;

    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua_message(raw: &str) -> (mlua::Lua, mlua::Table) {
        let lua = mlua::Lua::new();
        let message: Message = raw.parse().expect("valid irc message");
        let table = to_lua_message(&lua, &message).expect("message table");
        (lua, table)
    }

    #[test]
    fn privmsg_has_flat_shape() {
        let (_lua, table) = lua_message(":alice!~alice@example.com PRIVMSG #tirc :hello world\r\n");

        assert_eq!(table.get::<String>("command").unwrap(), "PRIVMSG");
        assert_eq!(table.get::<String>("nick").unwrap(), "alice");
        assert_eq!(table.get::<String>("user").unwrap(), "~alice");
        assert_eq!(table.get::<String>("host").unwrap(), "example.com");

        let params: mlua::Table = table.get("params").unwrap();
        assert_eq!(params.get::<String>(1).unwrap(), "#tirc");
        assert_eq!(params.get::<String>(2).unwrap(), "hello world");
    }

    #[test]
    fn server_prefix_sets_server() {
        let (_lua, table) = lua_message(":irc.example.com NOTICE * :hi there\r\n");

        assert_eq!(table.get::<String>("command").unwrap(), "NOTICE");
        assert_eq!(table.get::<String>("server").unwrap(), "irc.example.com");
        assert!(table.get::<Option<String>>("nick").unwrap().is_none());
    }

    #[test]
    fn numeric_reply_uses_symbolic_name() {
        let (_lua, table) = lua_message(":irc.example.com 001 alice :Welcome\r\n");

        assert_eq!(table.get::<String>("command").unwrap(), "RPL_WELCOME");

        let params: mlua::Table = table.get("params").unwrap();
        assert_eq!(params.get::<String>(1).unwrap(), "alice");
        assert_eq!(params.get::<String>(2).unwrap(), "Welcome");
    }

    #[test]
    fn tags_are_name_value_pairs() {
        let (_lua, table) =
            lua_message("@time=2026-06-26T00:00:00Z :alice PRIVMSG #tirc :hi\r\n");

        let tags: mlua::Table = table.get("tags").unwrap();
        let first: mlua::Table = tags.get(1).unwrap();
        assert_eq!(first.get::<String>(1).unwrap(), "time");
        assert_eq!(first.get::<String>(2).unwrap(), "2026-06-26T00:00:00Z");
    }

    #[test]
    fn tostring_yields_raw_line() {
        let (lua, table) = lua_message(":alice PRIVMSG #tirc :hello world\r\n");

        let tostring: mlua::Function = lua.globals().get("tostring").unwrap();
        let rendered: String = tostring.call(table).unwrap();
        assert_eq!(rendered, ":alice PRIVMSG #tirc :hello world");
    }
}
