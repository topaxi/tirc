//! The native `tirc.json` module: JSON encoding/decoding for Lua consumers
//! (the natural companion to `tirc.http.fetch`).

use mlua::{Lua, LuaSerdeExt};

use super::get_or_create_module;

/// Registers the `tirc.json` module. `json.decode(s)` parses JSON into Lua
/// values (objects/arrays as tables, null as nil); `json.encode(v)` serializes
/// a Lua value into a JSON string. Both raise on invalid input.
pub fn create_tirc_json_lua_module(lua: &Lua) -> anyhow::Result<mlua::Table> {
    let module = get_or_create_module(lua, "tirc.json")?;

    module.set(
        "decode",
        lua.create_function(|lua, s: mlua::String| {
            let value: serde_json::Value = serde_json::from_slice(&s.as_bytes())
                .map_err(|err| mlua::Error::runtime(format!("json.decode: {err}")))?;
            lua.to_value(&value)
        })?,
    )?;

    module.set(
        "encode",
        lua.create_function(|lua, value: mlua::Value| {
            let value: serde_json::Value = lua.from_value(value)?;
            serde_json::to_string(&value)
                .map_err(|err| mlua::Error::runtime(format!("json.encode: {err}")))
        })?,
    )?;

    Ok(module)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_roundtrips_nested_values_and_rejects_garbage() {
        let lua = Lua::new();
        create_tirc_json_lua_module(&lua).unwrap();
        let (name, first, encoded, err): (String, f64, String, bool) = lua
            .load(
                r#"
                local json = require('tirc.json')
                local v = json.decode('{"name":"tirc","list":[1,2,3]}')
                local encoded = json.encode({ a = { b = 42 } })
                local ok = pcall(json.decode, '{nope')
                return v.name, v.list[1], encoded, ok
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(name, "tirc");
        assert_eq!(first, 1.0);
        assert_eq!(encoded, r#"{"a":{"b":42}}"#);
        assert!(!err, "invalid json must raise");
    }
}
