use std::path::{Path, PathBuf};

use anyhow::anyhow;
use indoc::indoc;
use mlua::{IntoLuaMulti, Lua, LuaSerdeExt, Table, Value};
use serde::Deserialize;

use crate::{
    core::{BackendId, Protocol},
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

/// One configured backend. The required `protocol` selects which fields apply;
/// IRC fields and Matrix fields share this struct so a Lua config author fills in
/// only the relevant subset.
#[derive(Deserialize, Debug)]
pub struct ServerConfig {
    pub protocol: Protocol,

    // IRC fields.
    pub host: Option<String>,

    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default = "bool_true")]
    pub use_tls: bool,

    #[serde(default)]
    pub accept_invalid_cert: bool,

    #[serde(default)]
    pub nickname: Vec<String>,

    pub realname: Option<String>,

    #[serde(default)]
    pub autojoin: Vec<String>,

    // Matrix fields.
    pub homeserver: Option<String>,
    pub user_id: Option<String>,
    pub password: Option<String>,
    pub device_id: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct TircConfig {
    pub servers: Box<[ServerConfig]>,

    #[serde(default)]
    pub auto_reload_config: bool,

    #[serde(default)]
    pub watch_files: Vec<String>,
}

fn get_default_config() -> &'static str {
    indoc! {"
        local tirc = require('tirc')
        local theme = require('tirc.tui.themes.default')

        local config = tirc.create_config()

        config.servers = {
          {
            protocol = 'irc',
            host = 'irc.topaxi.ch',
            nickname = { 'Rincewind', 'Twoflower' },
            port = 6697,
            use_tls = true,
            autojoin = { '#tirc' },
            -- Free-form metadata passed back to Lua for rendering. The default
            -- theme uses `label` to shorten the buffer bar in multi-server mode.
            metadata = { label = 'topaxi' },
          },
        }

        tirc.use(theme)

        return config
    "}
}

/// The closed set of side-effect events themes/plugins can subscribe to via
/// `tirc.on(name, fn)`. Keeping this an enum (rather than formatting a registry
/// key from an arbitrary string on every emit) is the single source of truth for
/// valid event names and avoids a per-emit allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventName {
    /// A normalized [`ChatEvent`](crate::core::ChatEvent) arrived from a backend.
    Event,
}

impl EventName {
    /// The (static) registry key under which this event's handlers are stored.
    fn registry_key(self) -> &'static str {
        match self {
            EventName::Event => "tirc-event-event",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name {
            "event" => Some(EventName::Event),
            _ => None,
        }
    }
}

fn register_event(lua: &Lua, (name, func): (String, mlua::Function)) -> mlua::Result<()> {
    let event = EventName::parse(&name)
        .ok_or_else(|| mlua::Error::external(anyhow!("unknown event name: {name}")))?;
    let key = event.registry_key();

    match lua.named_registry_value::<mlua::Value>(key)? {
        mlua::Value::Table(tbl) => {
            let len = tbl.raw_len();
            tbl.set(len + 1, func)?;
        }
        _ => {
            let tbl = lua.create_table()?;
            tbl.set(1, func)?;
            lua.set_named_registry_value(key, tbl)?;
        }
    }

    // Track event name so reload_lua_theme can clear it
    let tracked: mlua::Value = lua.named_registry_value("tirc-registered-events")?;
    let tracked = match tracked {
        mlua::Value::Table(t) => t,
        _ => {
            let t = lua.create_table()?;
            lua.set_named_registry_value("tirc-registered-events", &t)?;
            t
        }
    };
    tracked.set(name, true)?;

    Ok(())
}

/// Dispatches a fire-and-forget event to every handler registered via
/// `tirc.on(name, ...)`. Handler return values are ignored.
pub fn emit_event<Args>(lua: &Lua, event: EventName, args: Args) -> mlua::Result<()>
where
    Args: IntoLuaMulti + Clone,
{
    if let mlua::Value::Table(tbl) = lua.named_registry_value(event.registry_key())? {
        for func in tbl.sequence_values::<mlua::Function>() {
            func?.call::<()>(args.clone())?;
        }
    }

    Ok(())
}

/// Returns the theme object stored as `tirc.ui`, or `None` when none is set.
fn ui_object(lua: &Lua) -> Option<Table> {
    match lua.named_registry_value::<Value>("tirc-ui").ok()? {
        Value::Table(tbl) => Some(tbl),
        _ => None,
    }
}

/// Backs the `tirc.ui` property getter; exposed to Lua as `_tirc.__get_ui`.
fn get_ui(lua: &Lua, _: ()) -> mlua::Result<Value> {
    lua.named_registry_value::<Value>("tirc-ui")
}

/// Backs the `tirc.ui` property setter; exposed to Lua as `_tirc.__set_ui`.
///
/// Stores `value` as the theme object verbatim, preserving its metatable so Rust
/// can call its formatters method-style. Assigning `tirc.ui` replaces the whole
/// object; to combine themes, extend or patch the existing object in Lua rather
/// than relying on a merge here.
fn set_ui(lua: &Lua, value: Value) -> mlua::Result<()> {
    lua.set_named_registry_value("tirc-ui", value)
}

/// Invokes the UI formatter named `name` on the `tirc.ui` theme object.
///
/// Returns `None` when no theme or no such formatter is set, otherwise the
/// formatter's `mlua::Result` (an `Err` if the Lua callback raised). The caller
/// is responsible for rendering errors.
///
/// Formatters are called method-style: the `tirc.ui` object is passed as the
/// receiver (the implicit `self` of a `:` method) ahead of `args`, so a formatter
/// can use `self` to reach sibling methods and styles.
pub fn call_formatter<Args>(lua: &Lua, name: &str, args: Args) -> Option<mlua::Result<mlua::Value>>
where
    Args: IntoLuaMulti,
{
    let ui = ui_object(lua)?;
    let func: mlua::Function = match ui.get(name) {
        Ok(Some(func)) => func,
        _ => return None,
    };

    let mut args = match args.into_lua_multi(lua) {
        Ok(args) => args,
        Err(err) => return Some(Err(err)),
    };
    args.push_front(mlua::Value::Table(ui));

    Some(func.call(args))
}

/// Registry key holding the per-backend metadata table (`id -> metadata`).
const BACKEND_METADATA_KEY: &str = "tirc-backend-metadata";

/// Returns the `tirc-backend-metadata` registry table, creating it on first
/// access. Maps `BackendId.0` (integer) to the server's `metadata` Lua table.
fn backend_metadata_registry(lua: &Lua) -> mlua::Result<Table> {
    match lua.named_registry_value::<Value>(BACKEND_METADATA_KEY)? {
        Value::Table(tbl) => Ok(tbl),
        _ => {
            let tbl = lua.create_table()?;
            lua.set_named_registry_value(BACKEND_METADATA_KEY, &tbl)?;
            Ok(tbl)
        }
    }
}

/// Stores the `metadata` value for `id` so themes can read it back while
/// rendering. The value is kept as-is (an arbitrary Lua table), never copied.
pub fn set_backend_metadata(lua: &Lua, id: BackendId, value: Value) -> mlua::Result<()> {
    backend_metadata_registry(lua)?.set(id.0, value)
}

/// Returns the stored metadata table for `id`, or `None` when the backend has no
/// metadata. Used by the render helpers to attach `backend.metadata`.
pub fn get_backend_metadata(lua: &Lua, id: BackendId) -> Option<Value> {
    match backend_metadata_registry(lua)
        .ok()?
        .get::<Value>(id.0)
        .ok()?
    {
        Value::Nil => None,
        value => Some(value),
    }
}

/// Copies the `metadata` table of `config.servers[id + 1]` (Lua is 1-based) from
/// the evaluated `config` global into the per-backend store. A no-op when the
/// server entry carries no `metadata`. Relies on the existing identity that
/// `BackendId(index)` corresponds to the `index`-th configured server.
pub fn register_backend_metadata(lua: &Lua, id: BackendId) -> mlua::Result<()> {
    let Value::Table(config) = lua.globals().get::<Value>("config")? else {
        return Ok(());
    };
    let Value::Table(servers) = config.get::<Value>("servers")? else {
        return Ok(());
    };
    let Value::Table(server) = servers.get::<Value>(id.0 + 1)? else {
        return Ok(());
    };

    match server.get::<Value>("metadata")? {
        Value::Nil => Ok(()),
        metadata => set_backend_metadata(lua, id, metadata),
    }
}

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

/// Bundled Lua sources written to the config `types/` directory so an editor's
/// Lua language server can resolve `require('tirc.*')` and the `---@class` types
/// (`TircEvent`, `TircUi`, `TircTheme`, ...) when editing `init.lua`. Keyed by
/// their require path relative to `types/`.
const TYPE_DEFINITIONS: &[(&str, &str)] = &[
    ("tirc/init.lua", TIRC_INIT_LUA),
    ("tirc/config.lua", TIRC_CONFIG_LUA),
    ("tirc/utils.lua", TIRC_UTILS_LUA),
    ("tirc/class.lua", TIRC_CLASS_LUA),
    ("tirc/tui/theme.lua", TIRC_THEME_LUA),
    ("tirc/tui/themes/default.lua", TIRC_DEFAULT_THEME_LUA),
];

/// `.luarc.json` pointing the Lua language server at the exported definitions.
const LUARC_JSON: &str = r#"{
  "runtime": {
    "version": "LuaJIT",
    "path": ["?.lua", "?/init.lua", "types/?.lua", "types/?/init.lua"]
  },
  "workspace": {
    "library": ["types"],
    "checkThirdParty": false
  }
}
"#;

/// Exports the bundled Lua type definitions into `<config>/types/` and writes a
/// `.luarc.json` so an editor's Lua language server can type-check `init.lua`.
///
/// Each file is rewritten only when its content differs from the bundled copy, so
/// the definitions track the running binary without needless writes (which would
/// make the language server re-analyze). The `.luarc.json` is written once and
/// never clobbered, so a user's own language-server settings are preserved.
fn write_type_definitions(config_dir: &Path) -> anyhow::Result<()> {
    let types_dir = config_dir.join("types");

    for (relative, content) in TYPE_DEFINITIONS {
        let path = types_dir.join(relative);
        let up_to_date = std::fs::read_to_string(&path).is_ok_and(|existing| existing == *content);
        if !up_to_date {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, content)?;
        }
    }

    let luarc = config_dir.join(".luarc.json");
    if !luarc.exists() {
        std::fs::write(&luarc, LUARC_JSON)?;
    }

    Ok(())
}

/// Registers the `_tirc` runtime module and all builtin `tirc.*` Lua modules.
///
/// This is everything needed to evaluate a user config or a theme, without
/// touching the filesystem, which also makes it usable from tests.
pub fn register_builtin_modules(lua: &Lua) -> anyhow::Result<()> {
    let tirc_mod = get_or_create_module(lua, "_tirc")?;

    tirc_mod.set("version", get_version_lua_value(lua))?;
    tirc_mod.set("on", lua.create_function(register_event)?)?;
    tirc_mod.set("__get_ui", lua.create_function(get_ui)?)?;
    tirc_mod.set("__set_ui", lua.create_function(set_ui)?)?;

    create_date_time_module(lua)?;
    create_tirc_theme_lua_module(lua)?;

    let public_tirc_module: Table = lua
        .load(TIRC_INIT_LUA)
        .set_name("{builtin}/lua/tirc/init.lua")
        .call(())?;

    set_loaded_modules(lua, "tirc", public_tirc_module)?;

    let config_module: Table = lua
        .load(TIRC_CONFIG_LUA)
        .set_name("{builtin}/lua/tirc/config.lua")
        .call(())?;

    set_loaded_modules(lua, "tirc.config", config_module)?;

    let utils_module: Table = lua
        .load(TIRC_UTILS_LUA)
        .set_name("{builtin}/lua/tirc/utils.lua")
        .call(())?;

    set_loaded_modules(lua, "tirc.utils", utils_module)?;

    let class_module: Table = lua
        .load(TIRC_CLASS_LUA)
        .set_name("{builtin}/lua/tirc/class.lua")
        .call(())?;

    set_loaded_modules(lua, "tirc.class", class_module)?;

    let default_theme_module: Table = lua
        .load(TIRC_DEFAULT_THEME_LUA)
        .set_name("{builtin}/lua/tirc/tui/themes/default.lua")
        .call(())?;

    set_loaded_modules(lua, "tirc.tui.themes.default", default_theme_module)?;

    Ok(())
}

/// Registers builtins, sets config_dir, reads and evaluates the config file.
///
/// Does NOT touch package.path - that is set once in load_config and must
/// not be prepended again on reload (it would grow unbounded).
fn eval_config_file(lua: &Lua, config_path: &Path, config_dirname: &Path) -> anyhow::Result<Value> {
    register_builtin_modules(lua)?;

    let tirc_mod = get_or_create_module(lua, "_tirc")?;
    tirc_mod.set("config_dir", config_dirname.display().to_string())?;

    let config_lua_code = std::fs::read_to_string(config_path)?;
    let value: Value = lua
        .load(&config_lua_code)
        .set_name(config_path.display().to_string())
        .call(())?;

    Ok(value)
}

/// Reloads the Lua theme and non-server config from disk without restarting.
///
/// Clears the UI formatter table, all registered event handlers, and the
/// module cache, then re-evaluates the config file. Server config (the
/// returned TircConfig) is not re-read - only the Lua side is reset.
pub fn reload_lua_theme(lua: &Lua, config_path: &Path) -> anyhow::Result<()> {
    let config_dirname = config_path
        .parent()
        .ok_or_else(|| anyhow!("config path has no parent directory"))?;

    // Clear the UI formatter table so tirc.use(theme) starts from scratch
    lua.set_named_registry_value("tirc-ui", mlua::Value::Nil)?;

    // Clear all event handlers registered via tirc.on(name, fn)
    let tracked: mlua::Value = lua.named_registry_value("tirc-registered-events")?;
    if let mlua::Value::Table(tracked) = tracked {
        let names: Vec<String> = tracked
            .pairs::<String, mlua::Value>()
            .map(|r| r.map(|(k, _)| k))
            .collect::<mlua::Result<_>>()?;
        for name in names {
            let decorated = format!("tirc-event-{}", name);
            lua.set_named_registry_value(&decorated, mlua::Value::Nil)?;
        }
    }
    lua.set_named_registry_value("tirc-registered-events", mlua::Value::Nil)?;

    // Clear package.loaded in-place so user modules are re-required from disk.
    // In-place iteration-and-nil is used rather than table replacement because
    // LuaJIT's require resolves against the internal table object.
    {
        let loaded = crate::lua::get_loaded_modules(lua)?;
        let keys: Vec<mlua::Value> = loaded
            .pairs::<mlua::Value, mlua::Value>()
            .map(|r| r.map(|(k, _)| k))
            .collect::<mlua::Result<_>>()?;
        for key in keys {
            loaded.set(key, mlua::Value::Nil)?;
        }
    }

    eval_config_file(lua, config_path, config_dirname)?;

    Ok(())
}

/// Collects all Lua source files that should be watched for auto-reload.
///
/// Always includes `config_path` (the init.lua). Also scans `package.loaded`
/// for module names whose resolved file exists under `config_dir` - this
/// auto-discovers any files `require`d by the config without extra config.
/// Built-in modules (loaded from memory) have no file in the config dir and
/// are silently filtered out. Finally appends any paths from `extra_paths`
/// (resolved relative to `config_dir` if not absolute).
pub fn collect_user_watched_paths(
    lua: &Lua,
    config_dir: &Path,
    config_path: &Path,
    extra_paths: &[String],
) -> Vec<PathBuf> {
    let mut paths = vec![config_path.to_owned()];

    let module_names: Vec<String> = lua
        .globals()
        .get::<mlua::Table>("package")
        .ok()
        .and_then(|pkg| pkg.get::<mlua::Table>("loaded").ok())
        .map(|loaded| {
            loaded
                .pairs::<String, mlua::Value>()
                .filter_map(|r| r.ok().map(|(k, _)| k))
                .collect()
        })
        .unwrap_or_default();

    for name in module_names {
        let stem = name.replace('.', "/");
        for suffix in [".lua", "/init.lua"] {
            let candidate = config_dir.join(format!("{stem}{suffix}"));
            if candidate.exists() && !paths.contains(&candidate) {
                paths.push(candidate);
                break;
            }
        }
    }

    for extra in extra_paths {
        let path = if std::path::Path::new(extra).is_absolute() {
            PathBuf::from(extra)
        } else {
            config_dir.join(extra)
        };
        if path.exists() && !paths.contains(&path) {
            paths.push(path);
        }
    }

    paths
}

pub fn load_config(lua: &Lua) -> Result<(TircConfig, PathBuf), anyhow::Error> {
    let config_filename =
        xdg::BaseDirectories::with_prefix("tirc").place_config_file("init.lua")?;
    let config_dirname = config_filename
        .parent()
        .expect("Unable to create config directory");

    if !config_filename.exists() {
        std::fs::create_dir_all(config_dirname)?;
        std::fs::write(&config_filename, get_default_config())?;
    }

    // Best-effort: keep editor type definitions in sync. Never fatal - a
    // read-only config dir should not stop the client from starting.
    let _ = write_type_definitions(config_dirname);

    // Prepend the config directory to package.path exactly once. reload_lua_theme
    // does not touch package.path so the entry is never duplicated.
    {
        let globals = lua.globals();
        let package: Table = globals.get("package")?;
        let package_path: String = package.get("path")?;
        let mut path_array: Vec<String> = package_path.split(';').map(|s| s.to_owned()).collect();

        fn prefix_path(array: &mut Vec<String>, path: &Path) {
            array.insert(0, format!("{}/?.lua", path.display()));
            array.insert(1, format!("{}/?/init.lua", path.display()));
        }

        prefix_path(&mut path_array, config_dirname);
        package.set("path", path_array.join(";"))?;
    }

    let value = eval_config_file(lua, &config_filename, config_dirname)?;

    lua.globals().set("config", &value)?;

    let config = lua.from_value(value)?;

    Ok((config, config_filename))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::BackendInfo;
    use crate::core::{
        BackendId, ChatEvent, MembershipChange, MessageBody, MsgKind, Protocol, TargetId, UserRef,
    };
    use crate::tui::lua::to_lua_event;
    use crate::ui::StoredMessage;

    fn backend() -> BackendInfo {
        BackendInfo {
            id: BackendId(0),
            protocol: Protocol::Irc,
            name: "test".to_string(),
        }
    }

    fn stored(event: ChatEvent) -> StoredMessage {
        StoredMessage {
            time: chrono::Local::now(),
            event,
            pending: false,
            redacted: false,
            reactions: Default::default(),
        }
    }

    /// Renders a normalized event through the active theme's `message_text`
    /// formatter and returns the raw Lua result.
    fn render_message_text(lua: &Lua, event: ChatEvent) -> mlua::Value {
        let message = stored(event);
        let table = to_lua_event(lua, &message, &backend(), &TargetId::from("#tirc"), "#tirc")
            .expect("event table");

        call_formatter(lua, "message_text", (table, "me".to_string()))
            .expect("message_text formatter registered")
            .expect("message_text formatter callback")
    }

    fn setup_theme() -> Lua {
        let lua = Lua::new();
        register_builtin_modules(&lua).expect("builtin modules");

        lua.load("require('tirc.tui.themes.default').setup({})")
            .exec()
            .expect("theme setup");

        lua
    }

    #[test]
    fn register_backend_metadata_reads_from_config_global() {
        let lua = Lua::new();

        lua.load(indoc! {"
            config = {
              servers = {
                { protocol = 'irc', host = 'irc.topaxi.ch', metadata = { label = 'topaxi' } },
                { protocol = 'irc', host = 'irc.libera.chat' },
              },
            }
        "})
            .exec()
            .expect("set config global");

        register_backend_metadata(&lua, BackendId(0)).expect("register backend 0");
        register_backend_metadata(&lua, BackendId(1)).expect("register backend 1");

        let metadata = get_backend_metadata(&lua, BackendId(0)).expect("backend 0 has metadata");
        let Value::Table(metadata) = metadata else {
            panic!("expected a metadata table");
        };
        assert_eq!(metadata.get::<String>("label").unwrap(), "topaxi");

        // A server without `metadata` stores nothing.
        assert!(get_backend_metadata(&lua, BackendId(1)).is_none());
        // An unconfigured backend id has no metadata either.
        assert!(get_backend_metadata(&lua, BackendId(2)).is_none());
    }

    #[test]
    fn theme_renders_common_events_without_error() {
        let lua = setup_theme();

        let events = [
            ChatEvent::Message {
                target: TargetId::from("#tirc"),
                id: None,
                sender: UserRef::new("alice"),
                body: MessageBody::plain("hello #other world"),
                kind: MsgKind::Text,
                echo_of: None,
                time: None,
            },
            ChatEvent::Message {
                target: TargetId::from("#tirc"),
                id: None,
                sender: UserRef::new("alice"),
                body: MessageBody::plain("waves"),
                kind: MsgKind::Action,
                echo_of: None,
                time: None,
            },
            ChatEvent::Membership {
                target: TargetId::from("#tirc"),
                who: UserRef::new("alice"),
                change: MembershipChange::Join { realname: None },
                time: None,
            },
            ChatEvent::Membership {
                target: TargetId::from("#tirc"),
                who: UserRef::new("alice"),
                change: MembershipChange::Part { reason: None },
                time: None,
            },
            ChatEvent::ServerInfo {
                target: None,
                from: Some("irc.example.com".to_string()),
                code: Some("RPL_WELCOME".to_string()),
                text: "Welcome to the network".to_string(),
                raw: None,
            },
            ChatEvent::ServerInfo {
                target: Some(TargetId::from("#tirc")),
                from: Some("op".to_string()),
                code: Some("MODE".to_string()),
                text: "#tirc +o-v alice bob".to_string(),
                raw: None,
            },
        ];

        for event in events {
            let value = render_message_text(&lua, event.clone());
            assert!(
                matches!(value, mlua::Value::Table(_)),
                "expected a table of spans for {event:?}, got {value:?}"
            );
        }
    }

    #[test]
    fn theme_suppresses_roster_seeding() {
        let lua = setup_theme();

        let value = render_message_text(
            &lua,
            ChatEvent::Membership {
                target: TargetId::from("#tirc"),
                who: UserRef::new("alice"),
                change: MembershipChange::Present {
                    role: crate::core::MemberRole::Member,
                },
                time: None,
            },
        );
        assert!(matches!(value, mlua::Value::Nil));
    }

    #[test]
    fn type_definitions_are_exported_for_the_editor() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tirc-types-{nanos}"));

        write_type_definitions(&dir).expect("export type definitions");

        assert!(dir.join("types/tirc/init.lua").exists());
        assert!(dir.join("types/tirc/tui/theme.lua").exists());
        assert!(dir.join("types/tirc/tui/themes/default.lua").exists());
        assert!(dir.join(".luarc.json").exists());

        let init = std::fs::read_to_string(dir.join("types/tirc/init.lua")).unwrap();
        assert!(init.contains("---@class TircEvent"));
        let theme = std::fs::read_to_string(dir.join("types/tirc/tui/themes/default.lua")).unwrap();
        assert!(theme.contains("---@class TircTheme"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn theme_subclass_overrides_a_formatter_via_dispatch() {
        let lua = Lua::new();
        register_builtin_modules(&lua).expect("builtin modules");

        // A subclass overriding `format_message` must take effect even though
        // `message_text` (which dispatches to it) lives on the base class.
        // Subclasses use tirc.ui = Sub.new() directly; Sub.setup() is not
        // auto-generated and inherited Theme.setup() would instantiate Theme.
        lua.load(indoc! {"
            local tirc = require('tirc')
            local Default = require('tirc.tui.themes.default')
            local Sub = Default.extend()
            function Sub:format_message(_event)
              return { 'OVERRIDDEN' }
            end
            tirc.ui = Sub.new()
        "})
            .exec()
            .expect("subclass setup");

        let value = render_message_text(
            &lua,
            ChatEvent::Message {
                target: TargetId::from("#tirc"),
                id: None,
                sender: UserRef::new("alice"),
                body: MessageBody::plain("hi"),
                kind: MsgKind::Text,
                echo_of: None,
                time: None,
            },
        );

        match value {
            mlua::Value::Table(table) => {
                assert_eq!(table.get::<String>(1).unwrap(), "OVERRIDDEN");
            }
            other => panic!("expected overridden spans, got {other:?}"),
        }
    }

    #[test]
    fn collect_user_watched_paths_includes_config_and_user_modules() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tirc-watch-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();

        let config_path = dir.join("init.lua");
        std::fs::write(&config_path, "").unwrap();

        // A user module file that lives in the config dir.
        let module_file = dir.join("my_theme.lua");
        std::fs::write(&module_file, "return {}").unwrap();

        let lua = Lua::new();
        register_builtin_modules(&lua).expect("builtin modules");

        // Simulate the user module being required (put it into package.loaded).
        lua.load("package.loaded['my_theme'] = true")
            .exec()
            .unwrap();

        let paths = collect_user_watched_paths(&lua, &dir, &config_path, &[]);

        assert!(paths.contains(&config_path), "init.lua must be watched");
        assert!(
            paths.contains(&module_file),
            "user module file must be watched"
        );
        // Builtin modules (tirc, tirc.config, etc.) have no file under dir so
        // they must NOT appear in the list.
        assert_eq!(paths.len(), 2, "only init.lua and my_theme.lua expected");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_user_watched_paths_includes_extra_paths() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tirc-watch-extra-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();

        let config_path = dir.join("init.lua");
        std::fs::write(&config_path, "").unwrap();
        let extra = dir.join("colors.lua");
        std::fs::write(&extra, "return {}").unwrap();

        let lua = Lua::new();
        register_builtin_modules(&lua).expect("builtin modules");

        let paths =
            collect_user_watched_paths(&lua, &dir, &config_path, &["colors.lua".to_string()]);

        assert!(paths.contains(&config_path));
        assert!(paths.contains(&extra));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
