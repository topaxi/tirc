use std::str::FromStr;

use crate::config::get_or_create_module;
use irc::proto::Message;
use mlua::LuaSerdeExt;
use tui::style::Color;

fn get_tirc_theme_module(lua: &mlua::Lua) -> mlua::Table {
    get_or_create_module(lua, "tirc.tui.theme").expect("Unable to create tirc.theme module")
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
            let mut style = tui::style::Style::default();

            if let Ok(Some(color)) = tbl.get::<_, Option<String>>("fg") {
                style = style.fg(Color::from_str(&color).unwrap());
            }

            if let Ok(Some(color)) = tbl.get::<_, Option<String>>("bg") {
                style = style.bg(Color::from_str(&color).unwrap());
            }

            lua.to_value(&style)
        })?,
    )?;

    Ok(module)
}

pub fn to_lua_message<'lua>(
    lua: &'lua mlua::Lua,
    message: &Message,
) -> mlua::Result<mlua::Table<'lua>> {
    let lua_message = lua.to_value(message)?;

    match lua_message {
        mlua::Value::Table(table) => {
            let metatable = lua.create_table().expect("Unable to create metatable");
            let lua_message_str = lua.to_value(&message.to_string())?;

            metatable.set("__str", lua_message_str)?;
            metatable
                .set(
                    "__tostring",
                    lua.create_function(|_, lua_message: mlua::Value<'_>| {
                        Ok(match lua_message {
                            mlua::Value::Table(tbl) => tbl.get_metatable().unwrap().get("__str"),
                            _ => Ok(None::<String>),
                        })
                    })
                    .expect("Unable to create __tostring function"),
                )
                .expect("Unable to set __tostring function on metatable");

            table.set_metatable(Some(metatable));

            Ok(table)
        }
        _ => Err(mlua::Error::external(anyhow::anyhow!(
            "message must be a table"
        ))),
    }
}
