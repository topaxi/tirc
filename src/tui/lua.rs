use std::str::FromStr;
use std::sync::Arc;

use mlua::LuaSerdeExt;
use ratatui::style::Color;
use tokio::sync::mpsc;

use crate::backends::BackendInfo;
use crate::core::{
    ChatEvent, Command, MemberRole, MembershipChange, MessageBody, MsgKind, Protocol, TargetId,
    TxnAllocator, UserRef,
};
use crate::lua::get_or_create_module;
use crate::ui::{Member, StoredMessage};

/// Marker field set on a style table's metatable so the renderer can recognize a
/// styled span `{ value, style }` by identity instead of guessing from shape.
pub const STYLE_MARKER: &str = "__tirc_style";

fn get_tirc_theme_module(lua: &mlua::Lua) -> mlua::Table {
    get_or_create_module(lua, "tirc.tui.theme").expect("Unable to create tirc.tui.theme module")
}

fn parse_color(name: &str) -> mlua::Result<Color> {
    Color::from_str(name).map_err(|_| mlua::Error::external(format!("invalid color: {name}")))
}

/// Tags a serialized style table with the [`STYLE_MARKER`] metatable so the
/// renderer can distinguish a real style from any two-element table.
fn tag_style(lua: &mlua::Lua, style: ratatui::style::Style) -> mlua::Result<mlua::Value> {
    let value = lua.to_value(&style)?;

    if let mlua::Value::Table(table) = &value {
        let metatable = lua.create_table()?;
        metatable.set(STYLE_MARKER, true)?;
        table.set_metatable(Some(metatable))?;
    }

    Ok(value)
}

pub fn create_tirc_theme_lua_module(lua: &mlua::Lua) -> mlua::Result<mlua::Table> {
    let module = get_tirc_theme_module(lua);

    module.set(
        "color",
        lua.create_function(|lua, (r, g, b): (u8, u8, u8)| lua.to_value(&Color::Rgb(r, g, b)))?,
    )?;

    module.set(
        "color_from_str",
        lua.create_function(|lua, str: String| lua.to_value(&parse_color(&str)?))?,
    )?;

    module.set(
        "style",
        lua.create_function(|lua, tbl: mlua::Table| {
            let mut style = ratatui::style::Style::default();

            if let Ok(Some(color)) = tbl.get::<Option<String>>("fg") {
                style = style.fg(parse_color(&color)?);
            }

            if let Ok(Some(color)) = tbl.get::<Option<String>>("bg") {
                style = style.bg(parse_color(&color)?);
            }

            tag_style(lua, style)
        })?,
    )?;

    Ok(module)
}

fn protocol_str(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Irc => "irc",
        Protocol::Matrix => "matrix",
    }
}

fn role_str(role: MemberRole) -> &'static str {
    match role {
        MemberRole::Owner => "owner",
        MemberRole::Admin => "admin",
        MemberRole::Op => "op",
        MemberRole::HalfOp => "halfop",
        MemberRole::Voice => "voice",
        MemberRole::Member => "member",
    }
}

fn kind_str(kind: MsgKind) -> &'static str {
    match kind {
        MsgKind::Text => "text",
        MsgKind::Action => "action",
        MsgKind::Notice => "notice",
    }
}

fn user_table(lua: &mlua::Lua, user: &UserRef) -> mlua::Result<mlua::Table> {
    let table = lua.create_table()?;
    table.set("id", user.id.as_str())?;
    table.set("display", user.display.clone())?;
    table.set("name", user.name())?;
    Ok(table)
}

fn body_table(lua: &mlua::Lua, body: &MessageBody) -> mlua::Result<mlua::Table> {
    let table = lua.create_table()?;
    table.set("text", body.text.as_str())?;
    if let Some(crate::core::Formatted::Html(html)) = &body.formatted {
        table.set("html", html.as_str())?;
    }
    Ok(table)
}

/// Builds the Lua representation of a stored message that themes format.
///
/// The table is tagged with `type` (`message`, `membership`, `topic`, `rename`,
/// `quit`, `server_info`, ...) and carries `backend`, `target`, `pending`, and
/// `redacted` plus the per-variant payload. See `lua/tirc/init.lua` for the full
/// shape documented as `@class TircEvent`.
pub fn to_lua_event(
    lua: &mlua::Lua,
    message: &StoredMessage,
    backend: &BackendInfo,
    target: &TargetId,
    target_name: &str,
) -> mlua::Result<mlua::Table> {
    let table = lua.create_table()?;

    let backend_table = lua.create_table()?;
    backend_table.set("id", backend.id.0)?;
    backend_table.set("protocol", protocol_str(backend.protocol))?;
    backend_table.set("name", backend.name.as_str())?;
    if let Some(metadata) = crate::config::get_backend_metadata(lua, backend.id) {
        backend_table.set("metadata", metadata)?;
    }
    table.set("backend", backend_table)?;

    table.set("target", target.as_str())?;
    // Friendly buffer name (Matrix room name); equals `target` for IRC.
    table.set("target_name", target_name)?;
    table.set("pending", message.pending)?;
    table.set("redacted", message.redacted)?;

    if !message.reactions.is_empty() {
        let reactions = lua.create_table()?;
        for (key, count) in &message.reactions {
            reactions.set(key.as_str(), *count)?;
        }
        table.set("reactions", reactions)?;
    }

    match &message.event {
        ChatEvent::Message {
            sender, body, kind, ..
        } => {
            table.set("type", "message")?;
            table.set("sender", user_table(lua, sender)?)?;
            table.set("body", body_table(lua, body)?)?;
            table.set("kind", kind_str(*kind))?;
        }
        ChatEvent::Edit { body, .. } => {
            table.set("type", "edit")?;
            table.set("body", body_table(lua, body)?)?;
        }
        ChatEvent::Redaction { by, .. } => {
            table.set("type", "redaction")?;
            if let Some(by) = by {
                table.set("by", user_table(lua, by)?)?;
            }
        }
        ChatEvent::Reaction {
            sender, key, add, ..
        } => {
            table.set("type", "reaction")?;
            table.set("sender", user_table(lua, sender)?)?;
            table.set("key", key.as_str())?;
            table.set("add", *add)?;
        }
        ChatEvent::Membership { who, change, .. } => {
            table.set("type", "membership")?;
            table.set("who", user_table(lua, who)?)?;
            set_membership_change(lua, &table, change)?;
        }
        ChatEvent::Topic { who, topic, .. } => {
            table.set("type", "topic")?;
            table.set("topic", topic.as_str())?;
            if let Some(who) = who {
                table.set("who", user_table(lua, who)?)?;
            }
        }
        ChatEvent::BufferName { name, .. } => {
            table.set("type", "buffer_name")?;
            table.set("name", name.as_str())?;
        }
        ChatEvent::Rename { who, new_display } => {
            table.set("type", "rename")?;
            table.set("who", user_table(lua, who)?)?;
            table.set("new", new_display.as_str())?;
        }
        ChatEvent::Quit { who, reason } => {
            table.set("type", "quit")?;
            table.set("who", user_table(lua, who)?)?;
            table.set("reason", reason.clone())?;
        }
        ChatEvent::ServerInfo {
            from,
            code,
            text,
            raw,
            ..
        } => {
            table.set("type", "server_info")?;
            table.set("from", from.clone())?;
            table.set("code", code.clone())?;
            table.set("text", text.as_str())?;
            table.set("raw", raw.clone())?;
        }
    }

    Ok(table)
}

fn set_membership_change(
    lua: &mlua::Lua,
    table: &mlua::Table,
    change: &MembershipChange,
) -> mlua::Result<()> {
    match change {
        MembershipChange::Present { role } => {
            table.set("change", "present")?;
            table.set("role", role_str(*role))?;
        }
        MembershipChange::Join { realname } => {
            table.set("change", "join")?;
            table.set("realname", realname.clone())?;
        }
        MembershipChange::Part { reason } => {
            table.set("change", "part")?;
            table.set("reason", reason.clone())?;
        }
        MembershipChange::Kick { by, reason } => {
            table.set("change", "kick")?;
            table.set("by", user_table(lua, by)?)?;
            table.set("reason", reason.clone())?;
        }
        MembershipChange::Invite { by } => {
            table.set("change", "invite")?;
            table.set("by", user_table(lua, by)?)?;
        }
        MembershipChange::SetRole { role } => {
            table.set("change", "set_role")?;
            table.set("role", role_str(*role))?;
        }
    }

    Ok(())
}

/// Builds a backend-bound sender table exposing `send_message`/`send_notice` to
/// Lua `event` handlers. Sends enqueue protocol-agnostic [`Command`]s on the
/// backend's command channel; the enqueue is synchronous and non-blocking, so it
/// is safe from a Lua callback.
pub fn create_lua_sender(
    lua: &mlua::Lua,
    sender: mpsc::UnboundedSender<Command>,
    txn: Arc<TxnAllocator>,
) -> mlua::Result<mlua::Table> {
    let table = lua.create_table()?;

    let send = {
        let sender = sender.clone();
        let txn = Arc::clone(&txn);
        move |kind: MsgKind, target: String, message: String| {
            let _ = sender.send(Command::SendMessage {
                target: TargetId::from(target),
                body: message,
                kind,
                txn: txn.next(),
            });
        }
    };

    let message_send = send.clone();
    table.set(
        "send_message",
        lua.create_function(move |_, (target, message): (String, String)| {
            message_send(MsgKind::Text, target, message);
            Ok(())
        })?,
    )?;

    table.set(
        "send_notice",
        lua.create_function(move |_, (target, message): (String, String)| {
            send(MsgKind::Notice, target, message);
            Ok(())
        })?,
    )?;

    Ok(table)
}

/// Builds the Lua representation of a buffer member for the `user` formatter.
///
/// ```lua
/// { id = 'alice', display = nil, name = 'alice', role = 'op' }
/// ```
pub fn to_lua_user(lua: &mlua::Lua, member: &Member) -> mlua::Result<mlua::Table> {
    let table = user_table(lua, &member.user)?;
    table.set("role", role_str(member.role))?;
    // Back-compat alias used by older themes.
    table.set("nickname", member.user.name())?;
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{BackendId, EventId};

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

    #[test]
    fn message_event_has_flat_shape() {
        let lua = mlua::Lua::new();
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
        .unwrap();
        assert_eq!(table.get::<String>("type").unwrap(), "message");
        assert_eq!(table.get::<String>("kind").unwrap(), "text");

        let sender: mlua::Table = table.get("sender").unwrap();
        assert_eq!(sender.get::<String>("name").unwrap(), "alice");

        let body: mlua::Table = table.get("body").unwrap();
        assert_eq!(body.get::<String>("text").unwrap(), "hello");

        let backend_table: mlua::Table = table.get("backend").unwrap();
        assert_eq!(backend_table.get::<String>("protocol").unwrap(), "irc");
    }

    #[test]
    fn backend_metadata_is_exposed_on_the_event() {
        let lua = mlua::Lua::new();

        let metadata = lua.create_table().unwrap();
        metadata.set("label", "topaxi").unwrap();
        crate::config::set_backend_metadata(&lua, BackendId(0), mlua::Value::Table(metadata))
            .unwrap();

        let message = stored(ChatEvent::Message {
            target: TargetId::from("#tirc"),
            id: None,
            sender: UserRef::new("alice"),
            body: MessageBody::plain("hello"),
            kind: MsgKind::Text,
            echo_of: None,
            time: None,
        });

        let table =
            to_lua_event(&lua, &message, &backend(), &TargetId::from("#tirc"), "#tirc").unwrap();
        let backend_table: mlua::Table = table.get("backend").unwrap();
        let metadata: mlua::Table = backend_table.get("metadata").unwrap();
        assert_eq!(metadata.get::<String>("label").unwrap(), "topaxi");
    }

    #[test]
    fn server_info_keeps_code() {
        let lua = mlua::Lua::new();
        let message = stored(ChatEvent::ServerInfo {
            target: None,
            from: Some("irc.example.com".to_string()),
            code: Some("RPL_WELCOME".to_string()),
            text: "Welcome".to_string(),
            raw: None,
        });

        let table =
            to_lua_event(&lua, &message, &backend(), &TargetId::status(), "(status)").unwrap();
        assert_eq!(table.get::<String>("type").unwrap(), "server_info");
        assert_eq!(table.get::<String>("code").unwrap(), "RPL_WELCOME");
    }

    #[test]
    fn edit_targets_event_by_id() {
        let lua = mlua::Lua::new();
        let message = stored(ChatEvent::Edit {
            target: TargetId::from("!room:m"),
            id: EventId("$1".to_string()),
            body: MessageBody::plain("edited"),
        });

        let table = to_lua_event(
            &lua,
            &message,
            &backend(),
            &TargetId::from("!room:m"),
            "room",
        )
        .unwrap();
        assert_eq!(table.get::<String>("type").unwrap(), "edit");
        let body: mlua::Table = table.get("body").unwrap();
        assert_eq!(body.get::<String>("text").unwrap(), "edited");
    }

    #[test]
    fn style_is_tagged_with_marker() {
        let lua = mlua::Lua::new();
        create_tirc_theme_lua_module(&lua).unwrap();

        let style: mlua::Table = lua
            .load("require('tirc.tui.theme').style { fg = 'blue' }")
            .eval()
            .unwrap();

        let metatable = style.metatable().expect("style has metatable");
        assert!(metatable.get::<bool>(STYLE_MARKER).unwrap());
    }
}
