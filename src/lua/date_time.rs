use anyhow::anyhow;
use chrono::{Datelike, Timelike};
use mlua::Lua;

use super::get_or_create_module;

pub fn date_time_to_table<'a>(
    lua: &'a Lua,
    date_time: &chrono::DateTime<chrono::Local>,
) -> mlua::Result<mlua::Table<'a>> {
    let table = lua.create_table()?;
    table.set("year", date_time.year())?;
    table.set("month", date_time.month())?;
    table.set("day", date_time.day())?;
    table.set("hour", date_time.hour())?;
    table.set("minute", date_time.minute())?;
    table.set("second", date_time.second())?;
    Ok(table)
}

pub fn create_date_time_module(lua: &Lua) -> anyhow::Result<mlua::Table<'_>> {
    let module = get_or_create_module(lua, "tirc.date_time")?;

    module.set(
        "parse_from_str",
        lua.create_function(|lua, (date, format): (String, String)| {
            let date_time = chrono::DateTime::parse_from_str(&date, &format)
                .map_err(|err| mlua::Error::external(anyhow!(err)))?;
            date_time_to_table(lua, &date_time.into())
        })?,
    )?;

    module.set(
        "parse_from_rfc3339",
        lua.create_function(|lua, date: String| {
            let date_time = chrono::DateTime::parse_from_rfc3339(&date)
                .map_err(|err| mlua::Error::external(anyhow!(err)))?;
            date_time_to_table(lua, &date_time.into())
        })?,
    )?;

    Ok(module)
}
