//! The Lua runtime surface shared by the config loader, the renderer, and the
//! input layer: event handlers registered via `tirc.on`, the `tirc.ui` theme
//! object and its formatters, Lua completion sources, Lua user commands, and
//! per-backend metadata.
//! Everything lives in the Lua registry so it survives config reloads by
//! explicit reset ([`reset_runtime`]) rather than by recreating the `Lua`.

use anyhow::anyhow;
use mlua::{IntoLuaMulti, Lua, Table, Value};

use tirc_core::BackendId;

/// The closed set of side-effect events themes/plugins can subscribe to via
/// `tirc.on(name, fn)`. Keeping this an enum (rather than formatting a registry
/// key from an arbitrary string on every emit) is the single source of truth for
/// valid event names and avoids a per-emit allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventName {
    /// A normalized [`ChatEvent`](tirc_core::ChatEvent) arrived from a backend.
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

/// Backs the `tirc.log.*` helpers: routes a message from Lua through the `log`
/// facade (captured by the `:debug` pane). The `tirc::lua` target keeps Lua
/// output at the same verbosity as the rest of the crate under the default filter.
pub(crate) fn lua_log(_: &Lua, (level, message): (String, String)) -> mlua::Result<()> {
    match level.as_str() {
        "error" => log::error!(target: "tirc::lua", "{message}"),
        "warn" => log::warn!(target: "tirc::lua", "{message}"),
        "info" => log::info!(target: "tirc::lua", "{message}"),
        "debug" => log::debug!(target: "tirc::lua", "{message}"),
        _ => log::trace!(target: "tirc::lua", "{message}"),
    }
    Ok(())
}

pub(crate) fn register_event(
    lua: &Lua,
    (name, func): (String, mlua::Function),
) -> mlua::Result<()> {
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

    // Track event name so reset_runtime can clear it
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
pub(crate) fn get_ui(lua: &Lua, _: ()) -> mlua::Result<Value> {
    lua.named_registry_value::<Value>("tirc-ui")
}

/// Reads a string-sequence field from the `tirc.ui` theme object (through its
/// metatable chain, so class-level fields are found). `None` when no theme is
/// set or the field is absent/not a table. Used for theme-declared metadata
/// like `buffer_bar_styles`.
pub fn ui_string_list(lua: &Lua, name: &str) -> Option<Vec<String>> {
    let ui = ui_object(lua)?;
    match ui.get::<Value>(name).ok()? {
        Value::Table(list) => Some(
            list.sequence_values::<String>()
                .filter_map(Result::ok)
                .collect(),
        ),
        _ => None,
    }
}

/// Backs the `tirc.ui` property setter; exposed to Lua as `_tirc.__set_ui`.
///
/// Stores `value` as the theme object verbatim, preserving its metatable so Rust
/// can call its formatters method-style. Assigning `tirc.ui` replaces the whole
/// object; to combine themes, extend or patch the existing object in Lua rather
/// than relying on a merge here.
pub(crate) fn set_ui(lua: &Lua, value: Value) -> mlua::Result<()> {
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

/// Registry key holding the sequence of Lua completion-source specs.
const COMPLETION_SOURCES_KEY: &str = "tirc-completion-sources";

/// Returns the `tirc-completion-sources` registry table, creating it on first
/// access. A sequence of spec tables registered via
/// `tirc.register_completion_source`, consulted by the completion engine after
/// the builtin sources.
pub fn completion_sources_registry(lua: &Lua) -> mlua::Result<Table> {
    match lua.named_registry_value::<Value>(COMPLETION_SOURCES_KEY)? {
        Value::Table(tbl) => Ok(tbl),
        _ => {
            let tbl = lua.create_table()?;
            lua.set_named_registry_value(COMPLETION_SOURCES_KEY, &tbl)?;
            Ok(tbl)
        }
    }
}

/// Drops all registered Lua completion sources so a reload replaces them
/// instead of appending duplicates.
pub fn clear_completion_sources(lua: &Lua) -> mlua::Result<()> {
    lua.set_named_registry_value(COMPLETION_SOURCES_KEY, mlua::Value::Nil)
}

/// Backs `tirc.register_completion_source`: appends a completion-source spec
/// table (`{ name, mode, trigger, complete }`) to the registry. The spec is
/// validated lazily when the engine queries it, so registration itself never
/// fails on shape errors; missing core fields are rejected here to catch
/// typos early.
pub(crate) fn register_completion_source(lua: &Lua, spec: Table) -> mlua::Result<()> {
    if !spec.contains_key("mode")? {
        return Err(mlua::Error::external(anyhow!(
            "completion source is missing 'mode'"
        )));
    }
    if !matches!(spec.get::<Value>("trigger")?, Value::Table(_)) {
        return Err(mlua::Error::external(anyhow!(
            "completion source is missing a 'trigger' table"
        )));
    }
    if !matches!(spec.get::<Value>("complete")?, Value::Function(_)) {
        return Err(mlua::Error::external(anyhow!(
            "completion source is missing a 'complete' function"
        )));
    }
    let registry = completion_sources_registry(lua)?;
    registry.set(registry.raw_len() + 1, spec)?;
    Ok(())
}

/// Registry key holding the map of Lua user-command specs (`name -> spec`).
const USER_COMMANDS_KEY: &str = "tirc-user-commands";

/// Returns the `tirc-user-commands` registry table, creating it on first
/// access. Maps command name to its spec table
/// (`{ fn, nargs, complete, desc }`) registered via `tirc.create_command`.
pub fn user_commands_registry(lua: &Lua) -> mlua::Result<Table> {
    match lua.named_registry_value::<Value>(USER_COMMANDS_KEY)? {
        Value::Table(tbl) => Ok(tbl),
        _ => {
            let tbl = lua.create_table()?;
            lua.set_named_registry_value(USER_COMMANDS_KEY, &tbl)?;
            Ok(tbl)
        }
    }
}

/// The names of all registered Lua user commands, for dispatch resolution and
/// command-name completion.
pub fn user_command_names(lua: &Lua) -> Vec<String> {
    let Ok(registry) = user_commands_registry(lua) else {
        return Vec::new();
    };
    registry
        .pairs::<String, Value>()
        .filter_map(|pair| pair.ok().map(|(name, _)| name))
        .collect()
}

/// The stored spec table for a user command, or `None` when no command of
/// that name is registered.
pub fn user_command_spec(lua: &Lua, name: &str) -> Option<Table> {
    match user_commands_registry(lua).ok()?.get::<Value>(name).ok()? {
        Value::Table(spec) => Some(spec),
        _ => None,
    }
}

/// Drops all Lua user commands so a reload replaces them instead of keeping
/// stale handlers around.
pub fn clear_user_commands(lua: &Lua) -> mlua::Result<()> {
    lua.set_named_registry_value(USER_COMMANDS_KEY, mlua::Value::Nil)
}

/// Backs `tirc.create_command(name, fn, opts)`: registers a Lua user command,
/// nvim_create_user_command-style. `opts` accepts `nargs` (`'0'`, `'1'`,
/// `'?'`, `'*'`, `'+'`, or the integers 0/1; default `'0'`), `complete`
/// (a builtin kind - `'channel'`, `'nick'`, `'buffer'` - or a function
/// returning candidates), and `desc`. Re-registering a name overwrites the
/// previous command. Builtin commands always shadow user commands of the same
/// name; the user command remains reachable via an unambiguous prefix.
pub(crate) fn create_user_command(
    lua: &Lua,
    (name, func, opts): (String, mlua::Function, Option<Table>),
) -> mlua::Result<()> {
    let valid_name = !name.is_empty()
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid_name {
        return Err(mlua::Error::external(anyhow!(
            "invalid command name: {name:?} (expected [A-Za-z][A-Za-z0-9_]*)"
        )));
    }

    let spec = lua.create_table()?;
    spec.set("fn", func)?;

    let mut nargs = "0".to_string();
    if let Some(opts) = opts {
        nargs = match opts.get::<Value>("nargs")? {
            Value::Nil => nargs,
            Value::Integer(0) => "0".to_string(),
            Value::Integer(1) => "1".to_string(),
            Value::String(s) if matches!(&*s.to_string_lossy(), "0" | "1" | "?" | "*" | "+") => {
                s.to_string_lossy().to_string()
            }
            other => {
                return Err(mlua::Error::external(anyhow!(
                    "invalid nargs: {other:?} (expected '0', '1', '?', '*' or '+')"
                )))
            }
        };
        match opts.get::<Value>("complete")? {
            Value::Nil => {}
            value @ (Value::String(_) | Value::Function(_)) => spec.set("complete", value)?,
            other => {
                return Err(mlua::Error::external(anyhow!(
                    "invalid complete: {other:?} (expected a kind string or a function)"
                )))
            }
        }
        if let Value::String(desc) = opts.get::<Value>("desc")? {
            spec.set("desc", desc)?;
        }
    }
    spec.set("nargs", nargs)?;

    user_commands_registry(lua)?.set(name, spec)?;
    Ok(())
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

/// Resets the reload-scoped runtime state: the `tirc.ui` theme object, every
/// event handler registered via `tirc.on(name, fn)`, the Lua completion
/// sources, and the Lua user commands. Backend metadata is deliberately kept -
/// servers are not re-read on reload, so their metadata stays valid.
pub fn reset_runtime(lua: &Lua) -> mlua::Result<()> {
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

    // Clear Lua completion sources so :reload replaces them instead of
    // appending duplicates.
    clear_completion_sources(lua)?;

    // Clear Lua user commands so :reload replaces them instead of keeping
    // stale handlers around.
    clear_user_commands(lua)?;

    Ok(())
}
