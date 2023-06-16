use std::path::Path;

use indoc::indoc;
use mlua::{Lua, LuaSerdeExt, Table};
use serde::Deserialize;

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

    #[serde(default)]
    pub autojoin: Vec<String>,
}

#[derive(Deserialize, Debug)]
pub struct TircConfig {
    pub servers: Vec<ServerConfig>,
}

fn get_default_config() -> &'static str {
    let default_config = indoc! {"
        local config = {}

        config.servers = {
          {
            host = 'irc.topaxi.ch',
            nickname = { 'Rincewind', 'Twoflower' },
            port = 6697,
            use_tls = true,
            autojoin = { '#tirc' },
          },
        }

        return config
    "};

    default_config
}

pub async fn load_config() -> Result<(TircConfig, Lua), failure::Error> {
    let config_filename =
        xdg::BaseDirectories::with_prefix("tirc")?.place_config_file("init.lua")?;
    let config_dirname = config_filename
        .parent()
        .expect("Unable to create config directory");

    if !config_filename.exists() {
        std::fs::create_dir_all(&config_dirname)?;

        std::fs::write(&config_filename, get_default_config())?;
    }

    let config_lua_code = std::fs::read_to_string(&config_filename)?;

    let lua = Lua::new();

    let value = {
        let globals = lua.globals();

        let package: Table = globals.get("package")?;
        let package_path: String = package.get("path")?;
        let mut path_array: Vec<String> = package_path.split(";").map(|s| s.to_owned()).collect();

        fn prefix_path(array: &mut Vec<String>, path: &Path) {
            array.insert(0, format!("{}/?.lua", path.display()));
            array.insert(1, format!("{}/?/init.lua", path.display()));
        }

        prefix_path(&mut path_array, config_dirname);

        package.set("path", path_array.join(";"))?;

        lua.load(&config_lua_code)
            .set_name(config_filename.display().to_string())?
            .call(())?
    };

    let config: TircConfig = lua.from_value(value)?;

    Ok((config, lua))
}
