use std::path::{Path, PathBuf};

use anyhow::anyhow;
use indoc::indoc;
use mlua::{Lua, LuaSerdeExt, Table, Value};
use serde::Deserialize;

use crate::{
    core::Protocol,
    lua::builtins::{register_builtin_modules, TYPE_DEFINITIONS},
    lua::get_or_create_module,
    lua::runtime::reset_runtime,
};

pub mod aliases;
pub mod buffer_order;
pub mod ui_prefs;

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

    /// When `false` the server is skipped at startup. Defaults to `true`;
    /// omit from the Lua config to keep a server enabled.
    #[serde(default = "bool_true")]
    pub enabled: bool,

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

    /// Display aliases for buffers on this server: raw target -> shown name.
    #[serde(default)]
    pub aliases: std::collections::HashMap<String, String>,

    /// Explicit tab order for this server's buffers. Listed targets sort
    /// first, in list order (servers keep their config order relative to each
    /// other); unlisted buffers follow in arrival order. Include
    /// `"(status)"` to position the status buffer.
    #[serde(default)]
    pub buffer_order: Vec<String>,

    // Matrix fields.
    pub homeserver: Option<String>,
    pub user_id: Option<String>,
    pub password: Option<String>,
    pub device_id: Option<String>,

    // Mattermost fields.
    pub url: Option<String>,
    pub token: Option<String>,
    pub team: Option<String>,
}

/// How a left drag over the message area behaves. The release-capture copy-mode
/// toggle (a keybind) is always available regardless of this setting; this only
/// chooses the *default* drag behaviour.
#[derive(Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SelectionMode {
    /// A drag selects text inside the app and the yank keybind copies it to the
    /// clipboard. The default.
    #[default]
    App,
    /// The app does not select on drag; the user relies on the copy-mode toggle
    /// to release mouse capture and let the terminal do native selection.
    Native,
}

/// Which terminal graphics protocol to use for inline images. `Auto` queries the
/// terminal; the others force a specific protocol for terminals that misreport or
/// do not answer the query.
#[derive(Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImageProtocol {
    /// Detect the protocol (and font size) by querying the terminal. The default.
    #[default]
    Auto,
    Kitty,
    Sixel,
    Iterm2,
}

/// The default emoji set offered as quick reactions on a selected message when
/// the Lua config does not override `quick_reactions.emojis`.
fn default_quick_reaction_emojis() -> Vec<String> {
    ["👍", "❤️", "😂", "🎉", "😢", "🔥"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Quick-reaction affordance shown on the selected message. `enabled` gates the
/// whole feature (message-select mode and the pill bar); `emojis` is the ordered
/// set offered, bound to the number keys `1`..`9` in select mode.
#[derive(Deserialize, Debug, Clone)]
pub struct QuickReactions {
    #[serde(default = "bool_true")]
    pub enabled: bool,

    #[serde(default = "default_quick_reaction_emojis")]
    pub emojis: Vec<String>,
}

impl Default for QuickReactions {
    fn default() -> Self {
        QuickReactions {
            enabled: true,
            emojis: default_quick_reaction_emojis(),
        }
    }
}

#[derive(Deserialize, Debug)]
pub struct TircConfig {
    pub servers: Box<[ServerConfig]>,

    #[serde(default)]
    pub auto_reload_config: bool,

    #[serde(default)]
    pub watch_files: Vec<String>,

    /// Default mouse-drag selection behaviour. See [`SelectionMode`].
    #[serde(default)]
    pub selection_mode: SelectionMode,

    /// Terminal graphics protocol for inline images. See [`ImageProtocol`].
    #[serde(default)]
    pub image_protocol: ImageProtocol,

    /// Fetch Open Graph metadata for links in messages and render an inline
    /// preview (title/description, and a thumbnail when graphics are available).
    /// Enabled by default; set to `false` to avoid contacting linked servers.
    #[serde(default = "bool_true")]
    pub link_previews: bool,

    /// Quick reactions offered on the selected message. See [`QuickReactions`].
    #[serde(default)]
    pub quick_reactions: QuickReactions,
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

    // Clear the theme object, event handlers, and Lua completion sources so
    // tirc.use(theme) and re-registration start from scratch.
    reset_runtime(lua)?;

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
    use crate::core::backend::BackendInfo;
    use crate::core::{
        BackendId, ChatEvent, MembershipChange, MessageBody, MsgKind, Protocol, TargetId, UserRef,
    };
    use crate::lua::runtime::{
        call_formatter, get_backend_metadata, register_backend_metadata, ui_string_list,
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

    #[test]
    fn selection_mode_defaults_to_app() {
        assert_eq!(SelectionMode::default(), SelectionMode::App);
    }

    #[test]
    fn lua_completion_source_registers_and_completes() {
        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();

        lua.load(indoc::indoc! {r#"
            local tirc = require('tirc')
            tirc.register_completion_source {
              name = 'mentions',
              mode = 'insert',
              trigger = { kind = 'sigil', char = '@', min_chars = 1 },
              complete = function(ctx)
                return { { label = ctx.query, insert = ctx.query .. ': ' }, 'plain' }
              end,
            }
        "#})
            .exec()
            .unwrap();

        let mut engine = crate::ui::completion::CompletionEngine::new();
        let query = crate::ui::completion::CompletionQuery {
            mode: crate::ui::Mode::Insert,
            value: "hi @top",
            cursor: 7,
            force: false,
        };
        let (span, items) = engine.query(&query, &lua).expect("source should match");
        assert_eq!(span, (3, 7));
        assert_eq!(items[0].label, "top");
        assert_eq!(items[0].insert, "top: ");
        // A plain string is shorthand for both label and insert.
        assert_eq!(items[1].insert, "plain");

        // The registry is cleared on reload so sources do not accumulate.
        crate::lua::runtime::clear_completion_sources(&lua).unwrap();
        assert!(engine.query(&query, &lua).is_none());
    }

    #[test]
    fn lua_completion_source_rejects_bad_specs() {
        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();

        let result = lua
            .load("require('tirc').register_completion_source { mode = 'insert' }")
            .exec();
        assert!(result.is_err(), "missing trigger/complete must be rejected");
    }

    #[test]
    fn selection_mode_deserializes_lowercase() {
        let lua = Lua::new();
        let app: SelectionMode = lua.from_value(lua.load("'app'").eval().unwrap()).unwrap();
        let native: SelectionMode = lua
            .from_value(lua.load("'native'").eval().unwrap())
            .unwrap();
        assert_eq!(app, SelectionMode::App);
        assert_eq!(native, SelectionMode::Native);
    }

    #[test]
    fn quick_reactions_default_is_enabled_with_emojis() {
        let defaults = QuickReactions::default();
        assert!(defaults.enabled);
        assert_eq!(defaults.emojis, default_quick_reaction_emojis());
        assert!(!defaults.emojis.is_empty());
    }

    #[test]
    fn quick_reactions_deserialize_partial_and_disabled() {
        let lua = Lua::new();

        // A partial table fills the missing field from its default.
        let partial: QuickReactions = lua
            .from_value(lua.load("{ emojis = { '🚀', '👀' } }").eval().unwrap())
            .unwrap();
        assert!(partial.enabled, "enabled defaults to true when omitted");
        assert_eq!(partial.emojis, vec!["🚀".to_string(), "👀".to_string()]);

        let disabled: QuickReactions = lua
            .from_value(lua.load("{ enabled = false }").eval().unwrap())
            .unwrap();
        assert!(!disabled.enabled);
        assert_eq!(
            disabled.emojis,
            default_quick_reaction_emojis(),
            "emojis default even when only `enabled` is set"
        );
    }

    #[test]
    fn server_aliases_deserialize_and_default_to_empty() {
        let lua = Lua::new();

        let server: ServerConfig = lua
            .from_value(
                lua.load(
                    "{ protocol = 'irc', host = 'irc.libera.chat', aliases = { ['#a'] = 'x' } }",
                )
                .eval()
                .unwrap(),
            )
            .unwrap();
        assert_eq!(server.aliases.get("#a").map(String::as_str), Some("x"));

        let without: ServerConfig = lua
            .from_value(
                lua.load("{ protocol = 'irc', host = 'irc.libera.chat' }")
                    .eval()
                    .unwrap(),
            )
            .unwrap();
        assert!(without.aliases.is_empty());
    }

    #[test]
    fn server_buffer_order_deserializes_and_defaults_to_empty() {
        let lua = Lua::new();

        let server: ServerConfig = lua
            .from_value(
                lua.load(
                    "{ protocol = 'irc', host = 'irc.libera.chat', buffer_order = { '(status)', '#a' } }",
                )
                .eval()
                .unwrap(),
            )
            .unwrap();
        assert_eq!(server.buffer_order, ["(status)", "#a"]);

        let without: ServerConfig = lua
            .from_value(
                lua.load("{ protocol = 'irc', host = 'irc.libera.chat' }")
                    .eval()
                    .unwrap(),
            )
            .unwrap();
        assert!(without.buffer_order.is_empty());
    }

    fn stored(event: ChatEvent) -> StoredMessage {
        StoredMessage {
            time: chrono::Local::now(),
            event,
            pending: false,
            redacted: false,
            edited: false,
            reactions: Default::default(),
        }
    }

    /// Renders a normalized event through the active theme's `message_text`
    /// formatter and returns the raw Lua result.
    fn render_message_text(lua: &Lua, event: ChatEvent) -> mlua::Value {
        render_stored_message_text(lua, stored(event))
    }

    fn render_stored_message_text(lua: &Lua, message: StoredMessage) -> mlua::Value {
        let table = to_lua_event(lua, &message, &backend(), &TargetId::from("#tirc"), "#tirc")
            .expect("event table");

        call_formatter(lua, "message_text", (table, "me".to_string()))
            .expect("message_text formatter registered")
            .expect("message_text formatter callback")
    }

    /// Recursively collects every string from a nested Lua spans table.
    fn collect_text(value: &mlua::Value) -> String {
        match value {
            mlua::Value::String(s) => s.to_str().map(|s| s.to_owned()).unwrap_or_default(),
            mlua::Value::Table(table) => {
                let mut out = String::new();
                for i in 1.. {
                    match table.get::<mlua::Value>(i) {
                        Ok(mlua::Value::Nil) => break,
                        Ok(v) => out.push_str(&collect_text(&v)),
                        Err(_) => break,
                    }
                }
                out
            }
            _ => String::new(),
        }
    }

    fn setup_theme() -> Lua {
        let lua = Lua::new();
        register_builtin_modules(&lua).expect("builtin modules");

        lua.load("require('tirc.tui.themes.default'):setup({})")
            .exec()
            .expect("theme setup");

        lua
    }

    #[test]
    fn ui_string_list_reads_theme_class_field_through_metatable() {
        let lua = setup_theme();
        let styles = ui_string_list(&lua, "buffer_bar_styles").expect("theme declares styles");
        assert_eq!(styles, ["linear", "grouped", "per-backend", "tabbed"]);
        assert_eq!(
            ui_string_list(&lua, "no_such_field"),
            None,
            "absent fields yield None"
        );
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
                time: None,
            },
            ChatEvent::ServerInfo {
                target: Some(TargetId::from("#tirc")),
                from: Some("op".to_string()),
                code: Some("MODE".to_string()),
                text: "#tirc +o-v alice bob".to_string(),
                raw: None,
                time: None,
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
        // `tirc.use` calls `setup` method-style, so the inherited `setup`
        // instantiates the subclass (not the base Theme) and the override sticks.
        lua.load(indoc! {"
            local tirc = require('tirc')
            local Default = require('tirc.tui.themes.default')
            local Sub = Default.extend()
            function Sub:format_message(_event)
              return { 'OVERRIDDEN' }
            end
            tirc.use(Sub)
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

    #[test]
    fn theme_appends_edited_marker_to_message() {
        let lua = setup_theme();

        let mut message = stored(ChatEvent::Message {
            target: TargetId::from("#tirc"),
            id: None,
            sender: UserRef::new("alice"),
            body: MessageBody::plain("hello"),
            kind: MsgKind::Text,
            echo_of: None,
            time: None,
        });
        message.edited = true;

        let value = render_stored_message_text(&lua, message);
        let text = collect_text(&value);
        assert!(
            text.contains("(edited)"),
            "expected '(edited)' in rendered spans, got: {text:?}"
        );
    }

    #[test]
    fn theme_renders_reaction_pills() {
        use crate::ui::ReactionState;
        let lua = setup_theme();

        let mut message = stored(ChatEvent::Message {
            target: TargetId::from("#tirc"),
            id: None,
            sender: UserRef::new("alice"),
            body: MessageBody::plain("hello"),
            kind: MsgKind::Text,
            echo_of: None,
            time: None,
        });
        message.reactions.insert(
            "👍".to_string(),
            ReactionState {
                count: 2,
                mine: true,
            },
        );
        message.reactions.insert(
            "❤️".to_string(),
            ReactionState {
                count: 1,
                mine: false,
            },
        );

        let table = to_lua_event(
            &lua,
            &message,
            &backend(),
            &TargetId::from("#tirc"),
            "#tirc",
        )
        .expect("event table");
        let value = call_formatter(&lua, "render_reactions", (table, mlua::Value::Nil))
            .expect("render_reactions formatter registered")
            .expect("render_reactions formatter callback");

        // The formatter returns a list of `{ key, spans }` pills, sorted by key.
        let pills = match &value {
            mlua::Value::Table(t) => t,
            _ => panic!("render_reactions did not return a table"),
        };
        let mut text = String::new();
        for i in 1.. {
            match pills.get::<mlua::Value>(i) {
                Ok(mlua::Value::Table(pill)) => {
                    let spans: mlua::Value = pill.get("spans").expect("pill has spans");
                    text.push_str(&collect_text(&spans));
                }
                _ => break,
            }
        }
        assert!(text.contains("👍 2"), "expected '👍 2' pill, got: {text:?}");
        assert!(text.contains("❤️ 1"), "expected '❤️ 1' pill, got: {text:?}");
    }

    #[test]
    fn theme_renders_quick_reaction_pills() {
        let lua = setup_theme();

        let message = stored(ChatEvent::Message {
            target: TargetId::from("#tirc"),
            id: None,
            sender: UserRef::new("alice"),
            body: MessageBody::plain("hello"),
            kind: MsgKind::Text,
            echo_of: None,
            time: None,
        });

        let table = to_lua_event(
            &lua,
            &message,
            &backend(),
            &TargetId::from("#tirc"),
            "#tirc",
        )
        .expect("event table");
        let emojis = lua
            .create_sequence_from(["👍".to_string(), "🎉".to_string()])
            .expect("emoji list");
        let value = call_formatter(
            &lua,
            "render_quick_reactions",
            (table, emojis, mlua::Value::Nil),
        )
        .expect("render_quick_reactions formatter registered")
        .expect("render_quick_reactions formatter callback");

        let pills = match &value {
            mlua::Value::Table(t) => t,
            _ => panic!("render_quick_reactions did not return a table"),
        };
        let mut text = String::new();
        let mut keys = Vec::new();
        for i in 1.. {
            match pills.get::<mlua::Value>(i) {
                Ok(mlua::Value::Table(pill)) => {
                    keys.push(pill.get::<String>("key").expect("pill has key"));
                    let spans: mlua::Value = pill.get("spans").expect("pill has spans");
                    text.push_str(&collect_text(&spans));
                }
                _ => break,
            }
        }
        // Quick pills are styled like ordinary reaction pills and prefixed with
        // their number-key shortcut; `key` carries the emoji so a click toggles the
        // right reaction.
        assert!(text.contains("1 👍"), "expected '1 👍' pill, got: {text:?}");
        assert!(text.contains("2 🎉"), "expected '2 🎉' pill, got: {text:?}");
        assert_eq!(keys, vec!["👍".to_string(), "🎉".to_string()]);
    }

    #[test]
    fn quick_reactions_skip_already_reacted_emojis() {
        use crate::ui::ReactionState;
        let lua = setup_theme();

        let mut message = stored(ChatEvent::Message {
            target: TargetId::from("#tirc"),
            id: None,
            sender: UserRef::new("alice"),
            body: MessageBody::plain("hello"),
            kind: MsgKind::Text,
            echo_of: None,
            time: None,
        });
        // 👍 already has a reaction, so the quick pills must not re-offer it.
        message.reactions.insert(
            "👍".to_string(),
            ReactionState {
                count: 1,
                mine: false,
            },
        );

        let table = to_lua_event(
            &lua,
            &message,
            &backend(),
            &TargetId::from("#tirc"),
            "#tirc",
        )
        .expect("event table");
        let emojis = lua
            .create_sequence_from(["👍".to_string(), "🎉".to_string()])
            .expect("emoji list");
        let value = call_formatter(
            &lua,
            "render_quick_reactions",
            (table, emojis, mlua::Value::Nil),
        )
        .expect("formatter registered")
        .expect("formatter callback");

        let pills = match &value {
            mlua::Value::Table(t) => t,
            _ => panic!("did not return a table"),
        };
        let mut keys = Vec::new();
        for i in 1.. {
            match pills.get::<mlua::Value>(i) {
                Ok(mlua::Value::Table(pill)) => {
                    keys.push(pill.get::<String>("key").expect("pill has key"));
                }
                _ => break,
            }
        }
        assert_eq!(
            keys,
            vec!["🎉".to_string()],
            "already-reacted 👍 is skipped by the quick pills"
        );
    }
}
