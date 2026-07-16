//! The `tirc.tui.theme` Lua module: style construction helpers for themes.

use std::str::FromStr;

use mlua::LuaSerdeExt;
use ratatui::style::Color;

use super::get_or_create_module;

/// Marker field set on a style table's metatable so the renderer can recognize a
/// styled span `{ value, style }` by identity instead of guessing from shape.
pub const STYLE_MARKER: &str = "__tirc_style";

fn get_tirc_theme_module(lua: &mlua::Lua) -> mlua::Table {
    get_or_create_module(lua, "tirc.tui.theme").expect("Unable to create tirc.tui.theme module")
}

fn parse_color(name: &str) -> mlua::Result<Color> {
    Color::from_str(name).map_err(|_| mlua::Error::external(format!("invalid color: {name}")))
}

/// Tags a serialized style table with the [`STYLE_MARKER`] metatable so the
/// renderer can distinguish a real style from any two-element table.
fn tag_style(lua: &mlua::Lua, style: ratatui::style::Style) -> mlua::Result<mlua::Value> {
    let value = lua.to_value(&style)?;

    if let mlua::Value::Table(table) = &value {
        let metatable = lua.create_table()?;
        metatable.set(STYLE_MARKER, true)?;
        table.set_metatable(Some(metatable))?;
    }

    Ok(value)
}

/// Whether `table` is a style table produced by `tirc.tui.theme` (identified by
/// the [`STYLE_MARKER`] on its metatable).
pub fn is_style_table(table: &mlua::Table) -> bool {
    table
        .metatable()
        .and_then(|mt| mt.get::<Option<bool>>(STYLE_MARKER).ok().flatten())
        .unwrap_or(false)
}

pub fn create_tirc_theme_lua_module(lua: &mlua::Lua) -> mlua::Result<mlua::Table> {
    let module = get_tirc_theme_module(lua);

    module.set(
        "color",
        lua.create_function(|lua, (r, g, b): (u8, u8, u8)| lua.to_value(&Color::Rgb(r, g, b)))?,
    )?;

    module.set(
        "color_from_str",
        lua.create_function(|lua, str: String| lua.to_value(&parse_color(&str)?))?,
    )?;

    module.set(
        "style",
        lua.create_function(|lua, tbl: mlua::Table| {
            let mut style = ratatui::style::Style::default();

            if let Ok(Some(color)) = tbl.get::<Option<String>>("fg") {
                style = style.fg(parse_color(&color)?);
            }

            if let Ok(Some(color)) = tbl.get::<Option<String>>("bg") {
                style = style.bg(parse_color(&color)?);
            }

            tag_style(lua, style)
        })?,
    )?;

    Ok(module)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_is_tagged_with_marker() {
        let lua = mlua::Lua::new();
        create_tirc_theme_lua_module(&lua).unwrap();

        let style: mlua::Table = lua
            .load("require('tirc.tui.theme').style { fg = 'blue' }")
            .eval()
            .unwrap();

        let metatable = style.metatable().expect("style has metatable");
        assert!(metatable.get::<bool>(STYLE_MARKER).unwrap());
        assert!(is_style_table(&style));
    }
}
