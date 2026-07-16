//! The embedded builtin `tirc.*` Lua modules and the `_tirc` runtime module.
//!
//! In release builds everything here is filesystem-free (embedded strings
//! only), which makes it safe to call from tests. In debug builds the sources
//! are read from the repo so edits are hot-reloadable without recompiling.

use mlua::{Lua, Table};

use super::date_time::create_date_time_module;
use super::runtime::{get_ui, lua_log, register_completion_source, register_event, set_ui};
use super::theme::create_tirc_theme_lua_module;
use super::{get_or_create_module, set_loaded_modules};

fn get_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("Unable to parse version")
}

fn get_version_lua_value(lua: &Lua) -> mlua::Table {
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

    table
        .set_metatable(Some(metatable))
        .expect("Unable to set metatable");

    table
}

const TIRC_INIT_LUA: &str = include_str!("../../lua/tirc/init.lua");
const TIRC_CONFIG_LUA: &str = include_str!("../../lua/tirc/config.lua");
const TIRC_UTILS_LUA: &str = include_str!("../../lua/tirc/utils.lua");
const TIRC_CLASS_LUA: &str = include_str!("../../lua/tirc/class.lua");
const TIRC_THEME_LUA: &str = include_str!("../../lua/tirc/tui/theme.lua");
const TIRC_DEFAULT_THEME_LUA: &str = include_str!("../../lua/tirc/tui/themes/default.lua");
const TIRC_SLANTED_THEME_LUA: &str = include_str!("../../lua/tirc/tui/themes/slanted.lua");

/// Bundled Lua sources written to the config `types/` directory so an editor's
/// Lua language server can resolve `require('tirc.*')` and the `---@class` types
/// (`TircEvent`, `TircUi`, `TircTheme`, ...) when editing `init.lua`. Keyed by
/// their require path relative to `types/`.
pub const TYPE_DEFINITIONS: &[(&str, &str)] = &[
    ("tirc/init.lua", TIRC_INIT_LUA),
    ("tirc/config.lua", TIRC_CONFIG_LUA),
    ("tirc/utils.lua", TIRC_UTILS_LUA),
    ("tirc/class.lua", TIRC_CLASS_LUA),
    ("tirc/tui/theme.lua", TIRC_THEME_LUA),
    ("tirc/tui/themes/default.lua", TIRC_DEFAULT_THEME_LUA),
    ("tirc/tui/themes/slanted.lua", TIRC_SLANTED_THEME_LUA),
];

/// In debug (non-test) builds, reads a builtin Lua file from the source tree so
/// edits are picked up without recompiling. Falls back to the embedded string if
/// the file cannot be read (e.g. the binary has moved off the build machine).
/// In test and release builds the embedded string is always used so tests are
/// not affected by local WIP edits to the Lua files.
///
/// Returns `(chunk_name, source)` where `chunk_name` is the real path in debug
/// builds and the `{builtin}/...` sentinel in release/test builds.
fn load_builtin(
    relative: &str,
    embedded: &'static str,
) -> (String, std::borrow::Cow<'static, str>) {
    #[cfg(all(debug_assertions, not(test)))]
    {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
        if let Ok(content) = std::fs::read_to_string(&src) {
            return (src.display().to_string(), std::borrow::Cow::Owned(content));
        }
    }
    (
        format!("{{builtin}}/{relative}"),
        std::borrow::Cow::Borrowed(embedded),
    )
}

/// Returns the absolute paths to all builtin Lua source files in the repo.
///
/// Only available in debug (non-test) builds where `CARGO_MANIFEST_DIR` points
/// at the live source tree. Used to include builtins in the file-watch list so
/// that edits trigger a hot reload without recompiling.
#[cfg(all(debug_assertions, not(test)))]
pub fn builtin_lua_paths() -> Vec<std::path::PathBuf> {
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    [
        "lua/tirc/init.lua",
        "lua/tirc/config.lua",
        "lua/tirc/utils.lua",
        "lua/tirc/class.lua",
        "lua/tirc/tui/theme.lua",
        "lua/tirc/tui/themes/default.lua",
        "lua/tirc/tui/themes/slanted.lua",
    ]
    .iter()
    .map(|p| base.join(p))
    .filter(|p| p.exists())
    .collect()
}

/// Registers the `_tirc` runtime module and all builtin `tirc.*` Lua modules.
///
/// In release builds this is filesystem-free (embedded strings only), which
/// makes it safe to call from tests. In debug builds the sources are read from
/// the repo so edits are hot-reloadable without recompiling.
pub fn register_builtin_modules(lua: &Lua) -> anyhow::Result<()> {
    let tirc_mod = get_or_create_module(lua, "_tirc")?;

    tirc_mod.set("version", get_version_lua_value(lua))?;
    tirc_mod.set("on", lua.create_function(register_event)?)?;
    tirc_mod.set(
        "register_completion_source",
        lua.create_function(register_completion_source)?,
    )?;
    tirc_mod.set("__log", lua.create_function(lua_log)?)?;
    tirc_mod.set("__get_ui", lua.create_function(get_ui)?)?;
    tirc_mod.set("__set_ui", lua.create_function(set_ui)?)?;

    create_date_time_module(lua)?;
    create_tirc_theme_lua_module(lua)?;

    let (name, src) = load_builtin("lua/tirc/init.lua", TIRC_INIT_LUA);
    let public_tirc_module: Table = lua.load(src.as_ref()).set_name(name).call(())?;
    set_loaded_modules(lua, "tirc", public_tirc_module)?;

    let (name, src) = load_builtin("lua/tirc/config.lua", TIRC_CONFIG_LUA);
    let config_module: Table = lua.load(src.as_ref()).set_name(name).call(())?;
    set_loaded_modules(lua, "tirc.config", config_module)?;

    let (name, src) = load_builtin("lua/tirc/utils.lua", TIRC_UTILS_LUA);
    let utils_module: Table = lua.load(src.as_ref()).set_name(name).call(())?;
    set_loaded_modules(lua, "tirc.utils", utils_module)?;

    let (name, src) = load_builtin("lua/tirc/class.lua", TIRC_CLASS_LUA);
    let class_module: Table = lua.load(src.as_ref()).set_name(name).call(())?;
    set_loaded_modules(lua, "tirc.class", class_module)?;

    let (name, src) = load_builtin("lua/tirc/tui/themes/default.lua", TIRC_DEFAULT_THEME_LUA);
    let default_theme_module: Table = lua.load(src.as_ref()).set_name(name).call(())?;
    set_loaded_modules(lua, "tirc.tui.themes.default", default_theme_module)?;

    let (name, src) = load_builtin("lua/tirc/tui/themes/slanted.lua", TIRC_SLANTED_THEME_LUA);
    let slanted_theme_module: Table = lua.load(src.as_ref()).set_name(name).call(())?;
    set_loaded_modules(lua, "tirc.tui.themes.slanted", slanted_theme_module)?;

    Ok(())
}
