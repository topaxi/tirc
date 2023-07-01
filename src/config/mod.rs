use std::path::Path;

use anyhow::anyhow;
use indoc::indoc;
use mlua::{Lua, LuaSerdeExt, Table, ToLuaMulti, Value};
use serde::Deserialize;

use crate::{
    lua::{date_time::create_date_time_module, get_or_create_module, set_loaded_modules},
    tui::lua::create_tirc_theme_lua_module,
};

#[inline]
fn bool_true() -> bool {
    true
}

#[inline]
fn default_port() -> u16 {
    6697
}

#[derive(Deserialize, Debug)]
pub struct ServerConfig {
    pub host: String,

    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default = "bool_true")]
    pub use_tls: bool,

    #[serde(default)]
    pub accept_invalid_cert: bool,

    pub nickname: Vec<String>,
    pub realname: Option<String>,

    #[serde(default)]
    pub autojoin: Vec<String>,
}

#[derive(Deserialize, Debug)]
pub struct TircConfig {
    pub servers: Vec<ServerConfig>,
}

fn get_default_config() -> &'static str {
    indoc! {"
        local theme = require('tirc.tui.theme.default')

        local config = {
            debug = false,
        }

        config.servers = {
          {
            host = 'irc.topaxi.ch',
            nickname = { 'Rincewind', 'Twoflower' },
            port = 6697,
            use_tls = true,
            autojoin = { '#tirc' },
          },
        }

        theme.setup(config)

        return config
    "}
}

fn register_event(lua: &Lua, (name, func): (String, mlua::Function)) -> mlua::Result<()> {
    let decorated_name = format!("tirc-event-{}", name);
    let tbl: mlua::Value = lua.named_registry_value(&decorated_name)?;

    match tbl {
        mlua::Value::Nil => {
            let tbl = lua.create_table()?;
            tbl.set(1, func)?;
            lua.set_named_registry_value(&decorated_name, tbl)?;
            Ok(())
        }
        mlua::Value::Table(tbl) => {
            let len = tbl.raw_len();
            tbl.set(len + 1, func)?;
            Ok(())
        }
        _ => Err(mlua::Error::external(anyhow!(
            "registry key for {} has invalid type",
            decorated_name
        ))),
    }
}

pub fn emit_sync_callback<'lua, A>(
    lua: &'lua Lua,
    (name, args): (String, A),
) -> mlua::Result<mlua::Value<'lua>>
where
    A: ToLuaMulti<'lua>,
{
    let decorated_name = format!("tirc-event-{}", name);
    let tbl: mlua::Value = lua.named_registry_value(&decorated_name)?;

    match tbl {
        mlua::Value::Table(tbl) => {
            #[allow(clippy::never_loop)]
            for func in tbl.sequence_values::<mlua::Function>() {
                let func = func?;
                return func.call(args);
            }

            Ok(mlua::Value::Nil)
        }
        _ => Ok(mlua::Value::Nil),
    }
}

fn get_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("Unable to parse version")
}

fn get_version_lua_value(lua: &Lua) -> mlua::Table<'_> {
    let version = get_version();
    let table = lua.create_table().expect("Unable to create table");
    let metatable = lua.create_table().expect("Unable to create metatable");

    table
        .set("major", version.major)
        .expect("Unable to set major");
    table
        .set("minor", version.minor)
        .expect("Unable to set minor");
    table
        .set("patch", version.patch)
        .expect("Unable to set patch");

    metatable
        .set(
            "__tostring",
            lua.create_function(|_, version: mlua::Table| {
                let major: u8 = version.get("major").expect("Unable to get major");
                let minor: u8 = version.get("minor").expect("Unable to get minor");
                let patch: u8 = version.get("patch").expect("Unable to get patch");

                Ok(format!("{}.{}.{}", major, minor, patch))
            })
            .expect("Unable to create __tostring function"),
        )
        .expect("Unable to set __tostring");

    table.set_metatable(Some(metatable));

    table
}

pub async fn load_config() -> Result<(TircConfig, Lua), anyhow::Error> {
    let config_filename =
        xdg::BaseDirectories::with_prefix("tirc")?.place_config_file("init.lua")?;
    let config_dirname = config_filename
        .parent()
        .expect("Unable to create config directory");

    if !config_filename.exists() {
        std::fs::create_dir_all(config_dirname)?;

        std::fs::write(&config_filename, get_default_config())?;
    }

    let config_lua_code = std::fs::read_to_string(&config_filename)?;

    let lua = Lua::new();

    let config = {
        let globals = lua.globals();
        let tirc_mod = get_or_create_module(&lua, "tirc")?;

        tirc_mod.set("config_dir", config_dirname.display().to_string())?;
        tirc_mod.set("version", get_version_lua_value(&lua))?;
        tirc_mod.set("on", lua.create_function(register_event)?)?;

        let package: Table = globals.get("package")?;
        let package_path: String = package.get("path")?;
        let mut path_array: Vec<String> = package_path.split(';').map(|s| s.to_owned()).collect();

        fn prefix_path(array: &mut Vec<String>, path: &Path) {
            array.insert(0, format!("{}/?.lua", path.display()));
            array.insert(1, format!("{}/?/init.lua", path.display()));
        }

        prefix_path(&mut path_array, config_dirname);

        package.set("path", path_array.join(";"))?;

        create_date_time_module(&lua)?;
        create_tirc_theme_lua_module(&lua)?;

        let utils_module: Table = lua
            .load(include_str!("../../lua/tirc/utils.lua"))
            .set_name("{builtin}/tirc/lua/utils.lua")?
            .call(())?;

        set_loaded_modules(&lua, "tirc.utils", utils_module)?;

        let default_theme_module: Table = lua
            .load(include_str!("../../lua/tirc/tui/default_theme.lua"))
            .set_name("{builtin}/tirc/tui/default_theme.lua")?
            .call(())?;

        set_loaded_modules(&lua, "tirc.tui.theme.default", default_theme_module)?;

        let value: Value = lua
            .load(&config_lua_code)
            .set_name(config_filename.display().to_string())?
            .call(())?;

        let config: TircConfig = lua.from_value(value.clone())?;

        globals.set("config", value)?;

        config
    };

    Ok((config, lua))
}
