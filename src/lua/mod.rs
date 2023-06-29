use mlua::{Lua, Table, Value};

pub mod date_time;

pub fn get_loaded_modules(lua: &Lua) -> mlua::Result<mlua::Table<'_>> {
    let globals = lua.globals();
    let package: Table = globals.get("package")?;
    let loaded: Table = package.get("loaded")?;
    Ok(loaded)
}

pub fn set_loaded_modules<'lua>(
    lua: &Lua,
    name: &str,
    module: mlua::Table<'lua>,
) -> anyhow::Result<mlua::Table<'lua>> {
    let loaded: Table = get_loaded_modules(lua)?;
    loaded.set(name, module.clone())?;
    Ok(module)
}

pub fn get_or_create_module<'lua>(lua: &'lua Lua, name: &str) -> anyhow::Result<mlua::Table<'lua>> {
    let loaded: Table = get_loaded_modules(lua)?;

    let module = loaded.get(name)?;
    match module {
        Value::Nil => set_loaded_modules(lua, name, lua.create_table()?),
        Value::Table(table) => Ok(table),
        wat => anyhow::bail!(
            "cannot register module {} as package.loaded.{} is already set to a value of type {}",
            name,
            name,
            wat.type_name()
        ),
    }
}
