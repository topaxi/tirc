//! Shared method metatables for the data tables Rust hands to Lua (events,
//! buffer tabs, datetimes). The metatables live under fixed named-registry
//! keys and are rebuilt by `register_builtin_modules` on every config reload,
//! so their `__index` always points at the freshly loaded method module.

use mlua::{Lua, Table, Value};

/// Registry key of the `TircEvent` method metatable (`tirc.event` module).
pub const EVENT_META_KEY: &str = "tirc-meta-event";
/// Registry key of the `TircBufferTab` method metatable (`tirc.buffer` module).
pub const BUFFER_META_KEY: &str = "tirc-meta-buffer";
/// Registry key of the `TircDateTime` metatable (native `__tostring`/`format`).
pub const DATE_TIME_META_KEY: &str = "tirc-meta-datetime";

/// Attaches the shared method metatable stored under `key` to `table`.
/// A missing metatable (builtins not registered) is not an error - the table
/// simply stays plain.
pub fn attach_method_metatable(lua: &Lua, table: &Table, key: &str) -> mlua::Result<()> {
    if let Value::Table(metatable) = lua.named_registry_value(key)? {
        table.set_metatable(Some(metatable))?;
    }
    Ok(())
}

/// Builds a `{ __index = methods }` metatable and stores it under `key`,
/// replacing any previous one (so a reload re-points `__index` at the new
/// module table).
pub fn register_method_metatable(lua: &Lua, key: &str, methods: &Table) -> mlua::Result<()> {
    let metatable = lua.create_table()?;
    metatable.set("__index", methods)?;
    lua.set_named_registry_value(key, metatable)
}
