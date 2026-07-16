use anyhow::anyhow;
use chrono::{Datelike, Timelike};
use mlua::Lua;

use super::get_or_create_module;
use super::meta::{attach_method_metatable, DATE_TIME_META_KEY};

pub fn date_time_to_table(
    lua: &Lua,
    date_time: &chrono::DateTime<chrono::Local>,
) -> mlua::Result<mlua::Table> {
    let table = lua.create_table()?;
    table.set("year", date_time.year())?;
    table.set("month", date_time.month())?;
    table.set("day", date_time.day())?;
    table.set("hour", date_time.hour())?;
    table.set("minute", date_time.minute())?;
    table.set("second", date_time.second())?;
    attach_method_metatable(lua, &table, DATE_TIME_META_KEY)?;
    Ok(table)
}

/// Reads the six `TircDateTime` fields back into a chrono value for
/// strftime-style formatting.
fn naive_date_time(table: &mlua::Table) -> mlua::Result<chrono::NaiveDateTime> {
    let date =
        chrono::NaiveDate::from_ymd_opt(table.get("year")?, table.get("month")?, table.get("day")?)
            .ok_or_else(|| mlua::Error::external(anyhow!("invalid date")))?;
    date.and_hms_opt(
        table.get("hour")?,
        table.get("minute")?,
        table.get("second")?,
    )
    .ok_or_else(|| mlua::Error::external(anyhow!("invalid time")))
}

/// Builds the `TircDateTime` metatable (`__tostring` and a strftime `format`
/// method) and stores it under [`DATE_TIME_META_KEY`]. Native closures, so it
/// never goes stale across reloads.
fn register_date_time_metatable(lua: &Lua) -> mlua::Result<()> {
    let methods = lua.create_table()?;
    methods.set(
        "format",
        lua.create_function(|_, (table, fmt): (mlua::Table, String)| {
            Ok(naive_date_time(&table)?.format(&fmt).to_string())
        })?,
    )?;

    let metatable = lua.create_table()?;
    metatable.set("__index", methods)?;
    metatable.set(
        "__tostring",
        lua.create_function(|_, table: mlua::Table| {
            Ok(naive_date_time(&table)?
                .format("%Y-%m-%d %H:%M:%S")
                .to_string())
        })?,
    )?;
    lua.set_named_registry_value(DATE_TIME_META_KEY, metatable)
}

pub fn create_date_time_module(lua: &Lua) -> anyhow::Result<mlua::Table> {
    register_date_time_metatable(lua)?;

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
