use std::path::{Path, PathBuf};

use anyhow::anyhow;
#[cfg(test)]
use indoc::indoc;
use mlua::{Lua, LuaSerdeExt, Table, Value};
use serde::Deserialize;

use tirc_core::Protocol;
use tirc_lua::builtins::{register_builtin_modules, type_definitions};
use tirc_lua::get_or_create_module;
use tirc_lua::runtime::reset_runtime;

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

/// Whether a Matrix server entry uses Simplified Sliding Sync (MSC4186).
/// `Auto` (the default) probes the homeserver's advertised capabilities; `On`
/// and `Off` force one driver, useful for testing and for servers that
/// misreport support.
#[derive(Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SlidingSync {
    #[default]
    Auto,
    On,
    Off,
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

    /// Sliding-sync selection for Matrix servers; ignored by other protocols.
    #[serde(default)]
    pub sliding_sync: SlidingSync,

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

    /// Reveal the internal debug log buffer on startup instead of only when
    /// `:debug` is invoked. Log lines are captured regardless; this controls
    /// whether the buffer's tab is shown from the start.
    #[serde(default)]
    pub debug_log: bool,
}

fn get_default_config() -> &'static str {
    include_str!("../default_init.lua")
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

/// Exports the bundled Lua type definitions into `<config>/types/` so an editor's
/// Lua language server can type-check `init.lua`.
///
/// Each file is rewritten only when its content differs from the bundled copy, so
/// the definitions track the running binary without needless writes (which would
/// make the language server re-analyze).
fn write_type_definitions(config_dir: &Path) -> anyhow::Result<()> {
    let types_dir = config_dir.join("types");

    for (relative, content) in type_definitions() {
        let path = types_dir.join(relative);
        let up_to_date = std::fs::read_to_string(&path).is_ok_and(|existing| existing == content);
        if !up_to_date {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, content)?;
        }
    }

    Ok(())
}

/// Writes a `.luarc.json` pointing the Lua language server at the exported type
/// definitions, but only when one does not already exist, so a user's own
/// language-server settings are never clobbered.
fn write_luarc_json(config_dir: &Path) -> anyhow::Result<()> {
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
        let loaded = tirc_lua::get_loaded_modules(lua)?;
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

    // Best-effort: keep editor type definitions in sync and drop a .luarc.json
    // if the user has none. Never fatal - a read-only config dir should not stop
    // the client from starting.
    let _ = write_type_definitions(config_dirname);
    let _ = write_luarc_json(config_dirname);

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
    use tirc_core::backend::BackendInfo;
    use tirc_core::{
        BackendId, ChatEvent, MembershipChange, MessageBody, MsgKind, Protocol, TargetId, UserRef,
    };
    use tirc_lua::runtime::{
        call_formatter, get_backend_metadata, register_backend_metadata, ui_string_list,
    };
    use tirc_ui::lua::to_lua_event;
    use tirc_ui::StoredMessage;

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

        let mut engine = tirc_ui::completion::CompletionEngine::new();
        let query = tirc_ui::completion::CompletionQuery {
            mode: tirc_ui::Mode::Insert,
            value: "hi @top",
            cursor: 7,
            force: false,
            state: None,
            focused: None,
        };
        let (span, items) = engine.query(&query, &lua).expect("source should match");
        assert_eq!(span, (3, 7));
        assert_eq!(items[0].label, "top");
        assert_eq!(items[0].insert, "top: ");
        // A plain string is shorthand for both label and insert.
        assert_eq!(items[1].insert, "plain");

        // The registry is cleared on reload so sources do not accumulate.
        tirc_lua::runtime::clear_completion_sources(&lua).unwrap();
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
    fn lua_user_command_registers_completes_and_clears() {
        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();

        lua.load(indoc::indoc! {r#"
            local tirc = require('tirc')
            tirc.create_command('deploy', function(ctx) end, {
              nargs = 1,
              complete = function(ctx)
                return { 'staging', 'production' }
              end,
              desc = 'Deploy an environment',
            })
            tirc.create_command('shrug', function(ctx) end, { nargs = '*' })
        "#})
            .exec()
            .unwrap();

        let mut names = tirc_lua::runtime::user_command_names(&lua);
        names.sort();
        assert_eq!(names, ["deploy", "shrug"]);

        // Name completion offers the Lua command with a trailing space.
        let mut engine = tirc_ui::completion::CompletionEngine::new();
        let query = tirc_ui::completion::CompletionQuery {
            mode: tirc_ui::Mode::Command,
            value: "depl",
            cursor: 4,
            force: false,
            state: None,
            focused: None,
        };
        let (span, items) = engine.query(&query, &lua).expect("name should complete");
        assert_eq!(span, (0, 4));
        let deploy = items.iter().find(|i| i.label == "deploy").unwrap();
        assert_eq!(deploy.insert, "deploy ");

        // Argument completion calls the command's complete function, even
        // when the command name is prefix-abbreviated.
        let query = tirc_ui::completion::CompletionQuery {
            mode: tirc_ui::Mode::Command,
            value: "dep sta",
            cursor: 7,
            force: false,
            state: None,
            focused: None,
        };
        let (span, items) = engine.query(&query, &lua).expect("arg should complete");
        assert_eq!(span, (4, 7));
        assert_eq!(items[0].insert, "staging");
        assert_eq!(items[1].insert, "production");

        // reset_runtime clears user commands like the other reload state.
        tirc_lua::runtime::reset_runtime(&lua).unwrap();
        assert!(tirc_lua::runtime::user_command_names(&lua).is_empty());
    }

    #[test]
    fn lua_user_command_builtin_complete_kind() {
        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();

        lua.load(indoc::indoc! {r#"
            require('tirc').create_command('close', function(ctx) end, {
              nargs = 1,
              complete = 'channel',
            })
        "#})
            .exec()
            .unwrap();

        let mut state = tirc_ui::State::new();
        state.register_backend(backend());
        state.apply(
            BackendId(0),
            ChatEvent::Message {
                target: TargetId::from("#rust"),
                id: None,
                sender: UserRef::new("alice"),
                body: MessageBody::plain("hi"),
                kind: MsgKind::Text,
                echo_of: None,
                time: None,
            },
        );
        let focused = tirc_core::BufferId::new(BackendId(0), "#rust");

        let mut engine = tirc_ui::completion::CompletionEngine::new();
        let query = tirc_ui::completion::CompletionQuery {
            mode: tirc_ui::Mode::Command,
            value: "close #r",
            cursor: 8,
            force: false,
            state: Some(&state),
            focused: Some(&focused),
        };
        let (_, items) = engine.query(&query, &lua).expect("kind should complete");
        assert_eq!(items[0].insert, "#rust");
    }

    #[test]
    fn lua_user_command_rejects_bad_specs() {
        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();
        let tirc = "require('tirc')";

        for bad in [
            // Name must be [A-Za-z][A-Za-z0-9_]*.
            format!("{tirc}.create_command('9bad', function() end)"),
            format!("{tirc}.create_command('', function() end)"),
            // The handler function is required.
            format!("{tirc}.create_command('x')"),
            // Unknown arity / complete shapes are rejected eagerly.
            format!("{tirc}.create_command('x', function() end, {{ nargs = 'lots' }})"),
            format!("{tirc}.create_command('x', function() end, {{ complete = 42 }})"),
        ] {
            assert!(lua.load(&bad).exec().is_err(), "must be rejected: {bad}");
        }
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
    fn tirc_dev_servers_deserialize_into_valid_server_configs() {
        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();

        let servers: Vec<ServerConfig> = lua
            .from_value(
                lua.load("return require('tirc.dev').servers")
                    .eval()
                    .unwrap(),
            )
            .unwrap();

        let protocols: Vec<Protocol> = servers.iter().map(|s| s.protocol).collect();
        assert_eq!(
            protocols,
            [
                Protocol::Irc,
                Protocol::Matrix,
                Protocol::Matrix,
                Protocol::Mattermost
            ]
        );

        let irc = &servers[0];
        assert_eq!(irc.host.as_deref(), Some("localhost"));
        assert!(!irc.use_tls);

        let matrix = &servers[1];
        assert_eq!(matrix.homeserver.as_deref(), Some("http://localhost:6167"));
        assert_eq!(matrix.sliding_sync, SlidingSync::Off);

        let matrix_sliding = &servers[2];
        assert_eq!(
            matrix_sliding.homeserver.as_deref(),
            Some("http://localhost:6168")
        );
        assert_eq!(matrix_sliding.sliding_sync, SlidingSync::On);

        let mattermost = &servers[3];
        assert_eq!(mattermost.url.as_deref(), Some("http://localhost:8065"));
        assert_eq!(mattermost.team.as_deref(), Some("testteam"));

        // `cargo test` builds with debug_assertions on, so this is the real
        // dev.lua module (not the release-mode native stand-in).
        let is_dev: bool = lua
            .load("return require('tirc.dev').is_dev()")
            .eval()
            .unwrap();
        assert!(is_dev);
    }

    #[test]
    fn server_sliding_sync_deserializes_and_defaults_to_auto() {
        let lua = Lua::new();

        let server: ServerConfig = lua
            .from_value(
                lua.load(
                    "{ protocol = 'matrix', homeserver = 'https://example.org', sliding_sync = 'on' }",
                )
                .eval()
                .unwrap(),
            )
            .unwrap();
        assert_eq!(server.sliding_sync, SlidingSync::On);

        let off: ServerConfig = lua
            .from_value(
                lua.load(
                    "{ protocol = 'matrix', homeserver = 'https://example.org', sliding_sync = 'off' }",
                )
                .eval()
                .unwrap(),
            )
            .unwrap();
        assert_eq!(off.sliding_sync, SlidingSync::Off);

        let without: ServerConfig = lua
            .from_value(
                lua.load("{ protocol = 'matrix', homeserver = 'https://example.org' }")
                    .eval()
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(without.sliding_sync, SlidingSync::Auto);
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
        let table = to_lua_event(
            lua,
            &message,
            &backend(),
            &TargetId::from("#tirc"),
            "#tirc",
            "me",
        )
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
                    role: tirc_core::MemberRole::Member,
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

        let init = std::fs::read_to_string(dir.join("types/tirc/init.lua")).unwrap();
        assert!(init.contains("---@class TircEvent"));
        let theme = std::fs::read_to_string(dir.join("types/tirc/tui/themes/default.lua")).unwrap();
        assert!(theme.contains("---@class TircTheme"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn luarc_json_is_written_once_and_never_clobbered() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tirc-luarc-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let luarc = dir.join(".luarc.json");

        write_luarc_json(&dir).expect("write .luarc.json");
        assert!(luarc.exists());
        assert_eq!(std::fs::read_to_string(&luarc).unwrap(), LUARC_JSON);

        // A user's own settings must survive a second run.
        std::fs::write(&luarc, "{ \"custom\": true }").unwrap();
        write_luarc_json(&dir).expect("keep existing .luarc.json");
        assert_eq!(
            std::fs::read_to_string(&luarc).unwrap(),
            "{ \"custom\": true }"
        );

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
        use tirc_ui::ReactionState;
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
            "me",
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
            "me",
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
        use tirc_ui::ReactionState;
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
            "me",
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

    /// Builds a plain channel/DM message event table with own nick "Rincewind".
    fn notify_event(lua: &Lua, sender: &str, target: &str, body: &str) -> mlua::Table {
        notify_stored_event(
            lua,
            stored(ChatEvent::Message {
                target: TargetId::from(target),
                id: None,
                sender: UserRef::new(sender),
                body: MessageBody::plain(body),
                kind: MsgKind::Text,
                echo_of: None,
                time: None,
            }),
            target,
        )
    }

    fn notify_stored_event(lua: &Lua, message: StoredMessage, target: &str) -> mlua::Table {
        to_lua_event(
            lua,
            &message,
            &backend(),
            &TargetId::from(target),
            target,
            "Rincewind",
        )
        .expect("event table")
    }

    /// Calls the notify plugin's pure `should_notify` with an injected context.
    /// `opts` is a Lua table literal; pass fully-formed options (no normalize).
    fn should_notify(
        lua: &Lua,
        event: &mlua::Table,
        terminal_focused: bool,
        focused_buffer: Option<&str>,
        opts: &str,
    ) -> bool {
        let f: mlua::Function = lua
            .load(format!(
                indoc::indoc! {"
                    local notify = require('tirc.plugins.notify')
                    return function(event, terminal_focused, focused_buffer)
                      return notify.should_notify(event, {{
                        terminal_focused = terminal_focused,
                        focused_buffer = focused_buffer,
                        opts = {},
                      }})
                    end
                "},
                opts
            ))
            .eval()
            .expect("should_notify wrapper");
        f.call((event, terminal_focused, focused_buffer))
            .expect("should_notify call")
    }

    const NOTIFY_DEFAULT_OPTS: &str = "{ dms = true, kinds = { text = true, action = true } }";

    #[test]
    fn notify_decision_matrix() {
        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();

        // Word-boundary mention, case-insensitive.
        let mention = notify_event(&lua, "alice", "#tirc", "hey rincewind, ping");
        assert!(should_notify(
            &lua,
            &mention,
            false,
            None,
            NOTIFY_DEFAULT_OPTS
        ));

        // Substring inside a longer word is not a mention.
        let inside_word = notify_event(&lua, "alice", "#tirc", "rincewinds unite");
        assert!(!should_notify(
            &lua,
            &inside_word,
            false,
            None,
            NOTIFY_DEFAULT_OPTS
        ));

        // Channel chatter without a mention stays silent.
        let chatter = notify_event(&lua, "alice", "#tirc", "hello world");
        assert!(!should_notify(
            &lua,
            &chatter,
            false,
            None,
            NOTIFY_DEFAULT_OPTS
        ));

        // Own messages (echoes) never notify, nick compared case-insensitively.
        let own = notify_event(&lua, "rincewind", "#tirc", "rincewind: note to self");
        assert!(!should_notify(&lua, &own, false, None, NOTIFY_DEFAULT_OPTS));

        // Pending optimistic echoes and redacted messages stay silent.
        for flag in ["pending", "redacted"] {
            let event = notify_event(&lua, "alice", "#tirc", "hi rincewind");
            event.set(flag, true).unwrap();
            assert!(
                !should_notify(&lua, &event, false, None, NOTIFY_DEFAULT_OPTS),
                "{flag} message must not notify"
            );
        }

        // Non-message events stay silent even in a DM-shaped buffer.
        let membership = notify_stored_event(
            &lua,
            stored(ChatEvent::Membership {
                target: TargetId::from("#tirc"),
                who: UserRef::new("alice"),
                change: MembershipChange::Join { realname: None },
                time: None,
            }),
            "#tirc",
        );
        assert!(!should_notify(
            &lua,
            &membership,
            false,
            None,
            NOTIFY_DEFAULT_OPTS
        ));

        // Notices are excluded by the default kinds, included when opted in.
        let notice = notify_stored_event(
            &lua,
            stored(ChatEvent::Message {
                target: TargetId::from("#tirc"),
                id: None,
                sender: UserRef::new("alice"),
                body: MessageBody::plain("rincewind: notice"),
                kind: MsgKind::Notice,
                echo_of: None,
                time: None,
            }),
            "#tirc",
        );
        assert!(!should_notify(
            &lua,
            &notice,
            false,
            None,
            NOTIFY_DEFAULT_OPTS
        ));
        assert!(should_notify(
            &lua,
            &notice,
            false,
            None,
            "{ dms = true, kinds = { text = true, action = true, notice = true } }"
        ));

        // Direct messages (non-channel target) notify without a mention...
        let dm = notify_event(&lua, "alice", "alice", "hi there");
        assert!(should_notify(&lua, &dm, false, None, NOTIFY_DEFAULT_OPTS));
        // ...unless DM notifications are disabled.
        assert!(!should_notify(
            &lua,
            &dm,
            false,
            None,
            "{ dms = false, kinds = { text = true, action = true } }"
        ));

        // Extra highlight patterns match case-insensitively.
        let pattern_hit = notify_event(&lua, "alice", "#tirc", "the TIRC build failed");
        assert!(should_notify(
            &lua,
            &pattern_hit,
            false,
            None,
            "{ dms = true, kinds = { text = true }, patterns = { 'tirc' } }"
        ));
    }

    #[test]
    fn notify_suppressed_only_when_terminal_and_buffer_focused() {
        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();

        // backend id 0 + target => "0:#tirc" (renderer's focused_buffer format).
        let event = notify_event(&lua, "alice", "#tirc", "rincewind: hi");

        assert!(!should_notify(
            &lua,
            &event,
            true,
            Some("0:#tirc"),
            NOTIFY_DEFAULT_OPTS
        ));
        assert!(should_notify(
            &lua,
            &event,
            true,
            Some("0:#other"),
            NOTIFY_DEFAULT_OPTS
        ));
        assert!(should_notify(
            &lua,
            &event,
            false,
            Some("0:#tirc"),
            NOTIFY_DEFAULT_OPTS
        ));
    }

    #[test]
    fn notify_setup_wires_event_handler_with_injected_executor() {
        use tirc_lua::runtime::{emit_event, EventName};

        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();

        lua.load(indoc::indoc! {"
            local tirc = require('tirc')
            tirc.use(require('tirc.plugins.notify'), {
              notify = function(summary, body, event)
                captured = { summary = summary, body = body }
              end,
            })
            require('_tirc').terminal_focused = false
        "})
            .exec()
            .unwrap();

        let event = notify_event(&lua, "alice", "#tirc", "hello Rincewind");
        let sender_stub = lua.create_table().unwrap();
        emit_event(&lua, EventName::Event, (event, sender_stub)).unwrap();

        let captured: mlua::Table = lua.globals().get("captured").expect("executor called");
        assert_eq!(captured.get::<String>("summary").unwrap(), "alice (#tirc)");
        assert_eq!(captured.get::<String>("body").unwrap(), "hello Rincewind");
    }

    /// Calls the away plugin's pure `should_reply` with an injected context.
    /// `ctx` is a Lua table literal with `away`, optional `last_reply`, `now`,
    /// and `opts` fields; pass fully-formed options (no normalize).
    fn should_reply(lua: &Lua, event: &mlua::Table, ctx: &str) -> bool {
        let f: mlua::Function = lua
            .load(format!(
                indoc::indoc! {"
                    local away = require('tirc.plugins.away')
                    return function(event)
                      return away.should_reply(event, {})
                    end
                "},
                ctx
            ))
            .eval()
            .expect("should_reply wrapper");
        f.call(event).expect("should_reply call")
    }

    const AWAY_IRC_CTX: &str =
        "{ away = true, last_reply = {}, now = 1000, opts = { protocols = { irc = true } } }";

    #[test]
    fn away_should_reply_decision_matrix() {
        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();

        // DMs (non-channel target) get a reply while away...
        let dm = notify_event(&lua, "alice", "alice", "hi there");
        assert!(should_reply(&lua, &dm, AWAY_IRC_CTX));
        // ...but not while back.
        assert!(!should_reply(
            &lua,
            &dm,
            "{ away = false, last_reply = {}, now = 1000, opts = { protocols = { irc = true } } }"
        ));

        // The protocol gate: test backend is IRC, so matrix-only opts skip it
        // (the default - servers RPL_AWAY natively).
        assert!(!should_reply(
            &lua,
            &dm,
            "{ away = true, last_reply = {}, now = 1000, opts = { protocols = { matrix = true } } }"
        ));

        // Own echoes never trigger a reply, nick compared case-insensitively.
        let own = notify_event(&lua, "rincewind", "alice", "note to self");
        assert!(!should_reply(&lua, &own, AWAY_IRC_CTX));

        // Pending optimistic echoes and redacted messages stay silent.
        for flag in ["pending", "redacted"] {
            let event = notify_event(&lua, "alice", "alice", "hi");
            event.set(flag, true).unwrap();
            assert!(
                !should_reply(&lua, &event, AWAY_IRC_CTX),
                "{flag} message must not get a reply"
            );
        }

        // Notices are automated replies themselves; answering them loops.
        let notice = notify_stored_event(
            &lua,
            stored(ChatEvent::Message {
                target: TargetId::from("alice"),
                id: None,
                sender: UserRef::new("alice"),
                body: MessageBody::plain("I am currently away"),
                kind: MsgKind::Notice,
                echo_of: None,
                time: None,
            }),
            "alice",
        );
        assert!(!should_reply(&lua, &notice, AWAY_IRC_CTX));

        // Non-message events stay silent.
        let membership = notify_stored_event(
            &lua,
            stored(ChatEvent::Membership {
                target: TargetId::from("#tirc"),
                who: UserRef::new("alice"),
                change: MembershipChange::Join { realname: None },
                time: None,
            }),
            "#tirc",
        );
        assert!(!should_reply(&lua, &membership, AWAY_IRC_CTX));

        // Channel messages only reply on a mention, and only when opted in.
        let mention = notify_event(&lua, "alice", "#tirc", "rincewind: around?");
        assert!(!should_reply(&lua, &mention, AWAY_IRC_CTX));
        let mention_ctx = "{ away = true, last_reply = {}, now = 1000, \
             opts = { protocols = { irc = true }, reply_to_mentions = true } }";
        assert!(should_reply(&lua, &mention, mention_ctx));
        let chatter = notify_event(&lua, "alice", "#tirc", "hello world");
        assert!(!should_reply(&lua, &chatter, mention_ctx));

        // Cooldown: a recent reply to the same sender suppresses, an old one
        // does not (default 300s).
        assert!(!should_reply(
            &lua,
            &dm,
            "{ away = true, last_reply = { ['0:alice'] = 900 }, now = 1000, \
               opts = { protocols = { irc = true } } }"
        ));
        assert!(should_reply(
            &lua,
            &dm,
            "{ away = true, last_reply = { ['0:alice'] = 600 }, now = 1000, \
               opts = { protocols = { irc = true } } }"
        ));
    }

    #[test]
    fn away_setup_tracks_state_and_replies() {
        use tirc_lua::runtime::{emit_event, EventName};

        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();

        lua.load(indoc::indoc! {"
            local tirc = require('tirc')
            replies = {}
            clock = 1000
            tirc.use(require('tirc.plugins.away'), {
              protocols = { irc = true },
              reply = function(event, text)
                replies[#replies + 1] = text
              end,
              now = function()
                return clock
              end,
            })
        "})
            .exec()
            .unwrap();

        let away = lua
            .load("return require('tirc.plugins.away')")
            .eval::<mlua::Table>()
            .unwrap();
        let is_away = || -> bool {
            away.get::<mlua::Function>("is_away")
                .unwrap()
                .call::<bool>(())
                .unwrap()
        };
        let reply_count = || -> usize {
            lua.globals()
                .get::<mlua::Table>("replies")
                .unwrap()
                .raw_len()
        };
        let send_dm = || {
            let event = notify_event(&lua, "alice", "alice", "you there?");
            let sender_stub = lua.create_table().unwrap();
            emit_event(&lua, EventName::Event, (event, sender_stub)).unwrap();
        };

        // Not away: no reply.
        send_dm();
        assert!(!is_away());
        assert_eq!(reply_count(), 0);

        // Away: the first DM gets the prefixed message.
        emit_event(&lua, EventName::Away, Some("gone fishing".to_string())).unwrap();
        send_dm();
        assert!(is_away());
        assert_eq!(reply_count(), 1);
        let text: String = lua
            .globals()
            .get::<mlua::Table>("replies")
            .unwrap()
            .get(1)
            .unwrap();
        assert_eq!(text, "[away] gone fishing");

        // The same sender within the cooldown is suppressed...
        send_dm();
        assert_eq!(reply_count(), 1);
        // ...but replied to again once the cooldown elapsed.
        lua.load("clock = clock + 400").exec().unwrap();
        send_dm();
        assert_eq!(reply_count(), 2);

        // Back: no more replies.
        emit_event(&lua, EventName::Away, None::<String>).unwrap();
        send_dm();
        assert!(!is_away());
        assert_eq!(reply_count(), 2);
    }

    #[test]
    fn away_setup_registers_back_command() {
        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();

        lua.load("require('tirc').use(require('tirc.plugins.away'))")
            .exec()
            .unwrap();

        assert_eq!(tirc_lua::runtime::user_command_names(&lua), ["back"]);
    }

    /// Calls a metatable method on an event table built by `to_lua_event`.
    fn call_event_method<R: mlua::FromLuaMulti>(
        lua: &Lua,
        event: &mlua::Table,
        method: &str,
        args: impl mlua::IntoLuaMulti,
    ) -> R {
        let f: mlua::Function = event.get(method).expect("method resolvable via metatable");
        let mut all = args.into_lua_multi(lua).unwrap();
        all.push_front(mlua::Value::Table(event.clone()));
        f.call(all).expect("method call")
    }

    #[test]
    fn event_metatable_methods() {
        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();

        // is_dm: channel vs query target.
        let channel = notify_event(&lua, "alice", "#tirc", "hello rincewind");
        let dm = notify_event(&lua, "alice", "alice", "hi");
        assert!(!call_event_method::<bool>(&lua, &channel, "is_dm", ()));
        assert!(call_event_method::<bool>(&lua, &dm, "is_dm", ()));

        // is_mention: word-boundary own-nick match, patterns, nil body.
        assert!(call_event_method::<bool>(&lua, &channel, "is_mention", ()));
        let inside_word = notify_event(&lua, "alice", "#tirc", "rincewinds unite");
        assert!(!call_event_method::<bool>(
            &lua,
            &inside_word,
            "is_mention",
            ()
        ));
        let patterns = lua.create_sequence_from(["tirc"]).unwrap();
        let pattern_hit = notify_event(&lua, "alice", "#tirc", "the TIRC build");
        assert!(call_event_method::<bool>(
            &lua,
            &pattern_hit,
            "is_mention",
            patterns
        ));
        let bodyless = notify_stored_event(
            &lua,
            stored(ChatEvent::Membership {
                target: TargetId::from("#tirc"),
                who: UserRef::new("rincewind"),
                change: MembershipChange::Join { realname: None },
                time: None,
            }),
            "#tirc",
        );
        assert!(!call_event_method::<bool>(
            &lua,
            &bodyless,
            "is_mention",
            ()
        ));

        // is_own: case-insensitive nick comparison.
        let own = notify_event(&lua, "rincewind", "#tirc", "note");
        assert!(call_event_method::<bool>(&lua, &own, "is_own", ()));
        assert!(!call_event_method::<bool>(&lua, &channel, "is_own", ()));

        // buffer_id matches the "<backend>:<target>" focused-buffer format.
        assert_eq!(
            call_event_method::<String>(&lua, &channel, "buffer_id", ()),
            "0:#tirc"
        );
    }

    #[test]
    fn buffer_metatable_methods() {
        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();

        // Hand-build a tab table and attach the registry metatable, mirroring
        // what the renderer's buffer_tab_table does.
        let tab: mlua::Table = lua
            .load(indoc::indoc! {"
                return {
                  id = '0:#tirc',
                  name = 'tirc',
                  backend_name = 'test',
                  backend_metadata = { label = 'topaxi' },
                }
            "})
            .eval()
            .unwrap();
        tirc_lua::meta::attach_method_metatable(&lua, &tab, tirc_lua::meta::BUFFER_META_KEY)
            .unwrap();

        let label: String = tab
            .get::<mlua::Function>("backend_label")
            .unwrap()
            .call(&tab)
            .unwrap();
        assert_eq!(label, "topaxi");
        tab.set("backend_metadata", mlua::Value::Nil).unwrap();
        let label: String = tab
            .get::<mlua::Function>("backend_label")
            .unwrap()
            .call(&tab)
            .unwrap();
        assert_eq!(label, "test");

        // is_focused compares against _tirc.focused_buffer.
        let is_focused = |tab: &mlua::Table| -> bool {
            tab.get::<mlua::Function>("is_focused")
                .unwrap()
                .call(tab)
                .unwrap()
        };
        assert!(!is_focused(&tab));
        get_or_create_module(&lua, "_tirc")
            .unwrap()
            .set("focused_buffer", "0:#tirc")
            .unwrap();
        assert!(is_focused(&tab));
    }

    #[test]
    fn date_time_metatable_formats() {
        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();

        let (formatted, stringified): (String, String) = lua
            .load(indoc::indoc! {"
                local dt = require('tirc.date_time').parse_from_rfc3339(
                  '2026-07-16T13:37:42+00:00'
                )
                return dt:format('%H:%M'), tostring(dt)
            "})
            .eval()
            .unwrap();
        // parse_from_rfc3339 converts into the local timezone, so only assert
        // on the stable parts.
        assert_eq!(formatted.len(), 5);
        assert!(stringified.starts_with("2026-07-1"), "{stringified}");
        assert!(stringified.ends_with(":42"), "{stringified}");
    }

    #[test]
    fn method_metatables_survive_reload() {
        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();

        // Simulate a reload: re-registering must re-point the stored
        // metatables at the freshly loaded method modules.
        register_builtin_modules(&lua).unwrap();

        let current: bool = lua
            .load(indoc::indoc! {"
                local event_module = require('tirc.event')
                local registry_index = ...
                return registry_index == event_module
            "})
            .call({
                let mt: mlua::Table = lua
                    .named_registry_value(tirc_lua::meta::EVENT_META_KEY)
                    .unwrap();
                mt.get::<mlua::Table>("__index").unwrap()
            })
            .unwrap();
        assert!(
            current,
            "metatable __index must track the freshly loaded module"
        );

        // And a table created after the reload dispatches correctly.
        let dm = notify_event(&lua, "alice", "alice", "hi");
        assert!(call_event_method::<bool>(&lua, &dm, "is_dm", ()));
    }

    #[test]
    fn promise_chains_catches_and_finalizes() {
        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();

        let (chained, caught, finalized, late): (i64, String, bool, i64) = lua
            .load(indoc! {"
                local Promise = require('tirc.promise')

                -- next() chains transform values; returned promises are adopted.
                local chained
                Promise.resolve(1)
                  :next(function(v) return v + 1 end)
                  :next(function(v) return Promise.resolve(v * 10) end)
                  :next(function(v) chained = v end)

                -- Errors raised in handlers reject the chained promise.
                local caught
                Promise.resolve('x')
                  :next(function() error('boom', 0) end)
                  :catch(function(err) caught = err end)

                -- finally runs on both paths and passes the state through.
                local finalized = false
                Promise.reject('nope')
                  :finally(function() finalized = true end)
                  :catch(function() end)

                -- Handlers attached after settlement fire immediately.
                local late
                local settled = Promise.resolve(42)
                settled:next(function(v) late = v end)

                return chained, caught, finalized, late
            "})
            .eval()
            .unwrap();

        assert_eq!(chained, 20);
        assert_eq!(caught, "boom");
        assert!(finalized);
        assert_eq!(late, 42);
    }

    #[test]
    fn promise_await_inside_async_resolves_and_reraises() {
        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();

        let (value, reraised, outer_rejected, outside_err): (i64, String, String, String) = lua
            .load(indoc! {"
                local Promise = require('tirc.promise')

                -- Await returns the settled value without suspending.
                local value
                Promise.async(function()
                  value = Promise.resolve(7):await()
                end)()

                -- A rejected promise re-raises inside the coroutine...
                local reraised
                Promise.async(function()
                  local ok, err = pcall(function()
                    Promise.reject('denied'):await()
                  end)
                  reraised = err
                end)()

                -- ...and an uncaught error rejects the async outer promise.
                local outer_rejected
                Promise.async(function()
                  Promise.reject('bubbles'):await()
                end)():catch(function(err) outer_rejected = err end)

                -- Await outside Promise.async raises.
                local ok, outside_err = pcall(function()
                  Promise.resolve(1):await()
                end)

                return value, reraised, outer_rejected, tostring(outside_err)
            "})
            .eval()
            .unwrap();

        assert_eq!(value, 7);
        assert_eq!(reraised, "denied");
        assert_eq!(outer_rejected, "bubbles");
        assert!(outside_err.contains("Promise.async"), "{outside_err}");
    }

    #[test]
    fn promise_await_suspends_until_late_settlement() {
        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();

        // Settle the awaited promise only after the async fn has suspended,
        // mirroring how a host task completes on a later loop iteration.
        let (before, after): (bool, i64) = lua
            .load(indoc! {"
                local Promise = require('tirc.promise')

                local resolve
                local pending = Promise.new(function(res) resolve = res end)

                local result
                Promise.async(function()
                  result = pending:await()
                end)()

                local before = result == nil
                resolve(9)
                return before, result
            "})
            .eval()
            .unwrap();

        assert!(before, "async fn must suspend on a pending promise");
        assert_eq!(after, 9);
    }

    /// Registers a Lua global `find_fg(spans, text)` that walks a span tree and
    /// returns the serialized `fg` of the innermost `{ text, style }` pair.
    fn register_find_fg(lua: &Lua) {
        lua.load(indoc! {"
            function find_fg(spans, text)
              if type(spans) ~= 'table' then
                return nil
              end
              if spans[1] == text and type(spans[2]) == 'table' then
                return spans[2].fg
              end
              for _, v in ipairs(spans) do
                local found = find_fg(v, text)
                if found then
                  return found
                end
              end
              return nil
            end
        "})
            .exec()
            .expect("find_fg helper");
    }

    /// The expected serialized `fg` for a nick id under the plugin's palette.
    fn expected_nick_fg(lua: &Lua, id: &str) -> String {
        lua.load(format!(
            r#"
            local nick_colors = require('tirc.plugins.nick_colors')
            local theme = require('tirc.tui.theme')
            local color = nick_colors.color_for('{id}', nick_colors.default_palette)
            return theme.style({{ fg = color }}).fg
            "#
        ))
        .eval()
        .expect("expected fg")
    }

    #[test]
    fn nick_colors_color_is_deterministic() {
        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();

        let (same, case_insensitive, differs): (bool, bool, bool) = lua
            .load(indoc! {"
                local nick_colors = require('tirc.plugins.nick_colors')
                local palette = nick_colors.default_palette
                local a = nick_colors.color_for('alice', palette)
                return a == nick_colors.color_for('alice', palette),
                  a == nick_colors.color_for('ALICE', palette),
                  a ~= nick_colors.color_for('alice2', palette)
            "})
            .eval()
            .unwrap();
        assert!(same, "same id must map to the same color");
        assert!(case_insensitive, "ids hash case-insensitively");
        assert!(differs, "different ids should get different colors");
    }

    #[test]
    fn nick_colors_styles_message_and_userlist_consistently() {
        let lua = setup_theme();
        lua.load("require('tirc').use(require('tirc.plugins.nick_colors'))")
            .exec()
            .expect("plugin setup");
        register_find_fg(&lua);
        let expected = expected_nick_fg(&lua, "alice");

        let spans = render_message_text(
            &lua,
            ChatEvent::Message {
                target: TargetId::from("#tirc"),
                id: None,
                sender: UserRef::new("alice"),
                body: MessageBody::plain("hello"),
                kind: MsgKind::Text,
                echo_of: None,
                time: None,
            },
        );
        let find_fg: mlua::Function = lua.globals().get("find_fg").unwrap();
        let message_fg: String = find_fg.call((&spans, "alice")).expect("nick span found");
        assert_eq!(message_fg, expected);

        let member = tirc_ui::Member {
            user: UserRef::new("alice"),
            role: tirc_core::MemberRole::Member,
        };
        let user_table = tirc_ui::lua::to_lua_user(&lua, &member).expect("user table");
        let user_spans = call_formatter(&lua, "user", user_table)
            .expect("user formatter registered")
            .expect("user formatter callback");
        let user_fg: String = find_fg
            .call((&user_spans, "alice"))
            .expect("userlist span found");
        assert_eq!(
            user_fg, expected,
            "message and userlist must agree on the color"
        );
    }

    #[test]
    fn nick_colors_skips_pending_messages() {
        let lua = setup_theme();
        lua.load("require('tirc').use(require('tirc.plugins.nick_colors'))")
            .exec()
            .expect("plugin setup");
        register_find_fg(&lua);

        let mut message = stored(ChatEvent::Message {
            target: TargetId::from("#tirc"),
            id: None,
            sender: UserRef::new("alice"),
            body: MessageBody::plain("hello"),
            kind: MsgKind::Text,
            echo_of: None,
            time: None,
        });
        message.pending = true;

        let spans = render_stored_message_text(&lua, message);
        let find_fg: mlua::Function = lua.globals().get("find_fg").unwrap();
        let fg: String = find_fg.call((&spans, "alice")).expect("nick span found");
        let darkgray: String = lua
            .load("return require('tirc.tui.theme').style({ fg = 'darkgray' }).fg")
            .eval()
            .unwrap();
        assert_eq!(fg, darkgray, "pending messages keep the dimmed nick");
    }

    #[test]
    fn nick_style_without_provider_falls_back_to_theme_style() {
        let lua = setup_theme();
        register_find_fg(&lua);

        let is_nil: bool = lua
            .load("return require('tirc').nick_style({ id = 'alice', name = 'alice' }) == nil")
            .eval()
            .unwrap();
        assert!(is_nil, "no provider yields nil");

        let spans = render_message_text(
            &lua,
            ChatEvent::Message {
                target: TargetId::from("#tirc"),
                id: None,
                sender: UserRef::new("alice"),
                body: MessageBody::plain("hello"),
                kind: MsgKind::Text,
                echo_of: None,
                time: None,
            },
        );
        let find_fg: mlua::Function = lua.globals().get("find_fg").unwrap();
        let fg: String = find_fg.call((&spans, "alice")).expect("nick span found");
        let blue: String = lua
            .load("return require('tirc.tui.theme').style({ fg = 'blue' }).fg")
            .eval()
            .unwrap();
        assert_eq!(fg, blue, "without a provider the theme's blue applies");
    }

    #[test]
    fn reset_runtime_clears_nick_style_provider() {
        let lua = Lua::new();
        register_builtin_modules(&lua).unwrap();

        lua.load("require('tirc').use(require('tirc.plugins.nick_colors'))")
            .exec()
            .expect("plugin setup");
        let registered: bool = lua
            .load("return require('tirc').nick_style({ id = 'alice', name = 'alice' }) ~= nil")
            .eval()
            .unwrap();
        assert!(registered);

        tirc_lua::runtime::reset_runtime(&lua).expect("reset");
        let cleared: bool = lua
            .load("return require('tirc').nick_style({ id = 'alice', name = 'alice' }) == nil")
            .eval()
            .unwrap();
        assert!(cleared, ":reload must drop the stale provider");
    }
}
