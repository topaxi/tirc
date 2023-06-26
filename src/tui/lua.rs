use std::str::FromStr;

use crate::config::get_or_create_module;
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
