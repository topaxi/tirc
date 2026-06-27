use std::path::Path;

use anyhow::anyhow;
use indoc::indoc;
use mlua::{IntoLuaMulti, Lua, LuaSerdeExt, Table, Value};
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
    pub servers: Box<[ServerConfig]>,
}

fn get_default_config() -> &'static str {
    indoc! {"
        local tirc = require('tirc')
        local theme = require('tirc.tui.themes.default')

        local config = tirc.create_config()

        config.servers = {
          {
            host = 'irc.topaxi.ch',
            nickname = { 'Rincewind', 'Twoflower' },
            port = 6697,
            use_tls = true,
            autojoin = { '#tirc' },
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
            Ok(())
        }
        _ => {
            let tbl = lua.create_table()?;
            tbl.set(1, func)?;
            lua.set_named_registry_value(key, tbl)?;
            Ok(())
        }
    }
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

/// Returns the `tirc-ui` registry table, creating it on first access so reads
/// and merges always have a backing store.
fn ui_registry_table(lua: &Lua) -> mlua::Result<Table> {
    match lua.named_registry_value::<Value>("tirc-ui")? {
        Value::Table(tbl) => Ok(tbl),
        _ => {
            let tbl = lua.create_table()?;
            lua.set_named_registry_value("tirc-ui", &tbl)?;
            Ok(tbl)
        }
    }
}

/// Backs the `tirc.ui` property getter; exposed to Lua as `_tirc.__get_ui`.
fn get_ui(lua: &Lua, _: ()) -> mlua::Result<Table> {
    ui_registry_table(lua)
}

/// Backs the `tirc.ui` property setter; exposed to Lua as `_tirc.__set_ui`.
///
/// Merges `value` into the stored `tirc-ui` table two levels deep: top-level
/// categories (e.g. `format`) merge, and within each category individual entries
/// merge. This lets a theme or plugin override just a subset of formatters.
fn set_ui(lua: &Lua, value: Table) -> mlua::Result<()> {
    let target = ui_registry_table(lua)?;

    for pair in value.pairs::<String, Value>() {
        let (key, value) = pair?;

        match value {
            Value::Table(category) => {
                let dst = match target.get::<Value>(key.as_str())? {
                    Value::Table(tbl) => tbl,
                    _ => {
                        let tbl = lua.create_table()?;
                        target.set(key.as_str(), &tbl)?;
                        tbl
                    }
                };

                for entry in category.pairs::<Value, Value>() {
                    let (entry_key, entry_value) = entry?;
                    dst.set(entry_key, entry_value)?;
                }
            }
            other => target.set(key, other)?,
        }
    }

    Ok(())
}

/// Invokes the UI formatter named `name` (registered under `tirc.ui.format`).
///
/// Returns `None` when no formatter is registered for `name`, otherwise the
/// formatter's `mlua::Result` (an `Err` if the Lua callback raised). The caller
/// is responsible for rendering errors.
pub fn call_formatter<Args>(lua: &Lua, name: &str, args: Args) -> Option<mlua::Result<mlua::Value>>
where
    Args: IntoLuaMulti,
{
    let ui = ui_registry_table(lua).ok()?;
    let format: Table = match ui.get("format") {
        Ok(Value::Table(tbl)) => tbl,
        _ => return None,
    };
    let func: mlua::Function = match format.get(name) {
        Ok(Some(func)) => func,
        _ => return None,
    };

    Some(func.call(args))
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
        .load(include_str!("../../lua/tirc/init.lua"))
        .set_name("{builtin}/lua/tirc/init.lua")
        .call(())?;

    set_loaded_modules(lua, "tirc", public_tirc_module)?;

    let config_module: Table = lua
        .load(include_str!("../../lua/tirc/config.lua"))
        .set_name("{builtin}/lua/tirc/config.lua")
        .call(())?;

    set_loaded_modules(lua, "tirc.config", config_module)?;

    let utils_module: Table = lua
        .load(include_str!("../../lua/tirc/utils.lua"))
        .set_name("{builtin}/lua/tirc/utils.lua")
        .call(())?;

    set_loaded_modules(lua, "tirc.utils", utils_module)?;

    let default_theme_module: Table = lua
        .load(include_str!("../../lua/tirc/tui/themes/default.lua"))
        .set_name("{builtin}/lua/tirc/tui/themes/default.lua")
        .call(())?;

    set_loaded_modules(lua, "tirc.tui.themes.default", default_theme_module)?;

    Ok(())
}

pub fn load_config(lua: &Lua) -> Result<TircConfig, anyhow::Error> {
    let config_filename =
        xdg::BaseDirectories::with_prefix("tirc").place_config_file("init.lua")?;
    let config_dirname = config_filename
        .parent()
        .expect("Unable to create config directory");

    if !config_filename.exists() {
        std::fs::create_dir_all(config_dirname)?;

        std::fs::write(&config_filename, get_default_config())?;
    }

    let config_lua_code = std::fs::read_to_string(&config_filename)?;

    let config = {
        let globals = lua.globals();
        let tirc_mod = get_or_create_module(lua, "_tirc")?;

        tirc_mod.set("config_dir", config_dirname.display().to_string())?;

        let package: Table = globals.get("package")?;
        let package_path: String = package.get("path")?;
        let mut path_array: Vec<String> = package_path.split(';').map(|s| s.to_owned()).collect();

        fn prefix_path(array: &mut Vec<String>, path: &Path) {
            array.insert(0, format!("{}/?.lua", path.display()));
            array.insert(1, format!("{}/?/init.lua", path.display()));
        }

        prefix_path(&mut path_array, config_dirname);

        package.set("path", path_array.join(";"))?;

        register_builtin_modules(lua)?;

        let value: Value = lua
            .load(&config_lua_code)
            .set_name(config_filename.display().to_string())
            .call(())?;

        globals.set("config", &value)?;

        lua.from_value(value)?
    };

    Ok(config)
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
        let table =
            to_lua_event(lua, &message, &backend(), &TargetId::from("#tirc")).expect("event table");

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
            },
            ChatEvent::Message {
                target: TargetId::from("#tirc"),
                id: None,
                sender: UserRef::new("alice"),
                body: MessageBody::plain("waves"),
                kind: MsgKind::Action,
                echo_of: None,
            },
            ChatEvent::Membership {
                target: TargetId::from("#tirc"),
                who: UserRef::new("alice"),
                change: MembershipChange::Join { realname: None },
            },
            ChatEvent::Membership {
                target: TargetId::from("#tirc"),
                who: UserRef::new("alice"),
                change: MembershipChange::Part { reason: None },
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
            },
        );
        assert!(matches!(value, mlua::Value::Nil));
    }
}
