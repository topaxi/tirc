//! Protocol-agnostic core domain model.
//!
//! These types are the boundary between protocol backends (IRC, Matrix, ...) and
//! the rest of the application. Backends translate their wire protocol into
//! [`ChatEvent`]s and accept [`Command`]s; the UI, state, and Lua theme layers
//! only ever see these normalized types, never a protocol-specific message.
//!
//! Everything here is `serde`-serializable so the same model can later double as
//! a relay/wire protocol if the TUI is split from the core into separate
//! processes.

use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub mod backend;
pub mod logging;

/// The protocol a backend speaks. Exposed to themes as `event.backend.protocol`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Irc,
    Matrix,
    Mattermost,
}

/// Identifies one connected network. Several backends (multiple IRC servers and
/// Matrix homeservers) can run concurrently, each with a distinct id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BackendId(pub usize);

/// A conversation target within a backend: an IRC channel/nick or a Matrix room
/// id. Opaque to the core; only the owning backend interprets it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TargetId(pub String);

impl TargetId {
    /// The per-backend status/server buffer that holds messages with no specific
    /// conversation (server notices, numerics, connection lifecycle).
    pub const STATUS: &'static str = "(status)";

    pub fn status() -> Self {
        TargetId(Self::STATUS.to_string())
    }

    pub fn is_status(&self) -> bool {
        self.0 == Self::STATUS
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for TargetId {
    fn from(value: &str) -> Self {
        TargetId(value.to_string())
    }
}

impl From<String> for TargetId {
    fn from(value: String) -> Self {
        TargetId(value)
    }
}

/// A buffer is one conversation on one backend. Replaces the old bare-`String`
/// buffer keys so two networks can have same-named channels without collision.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BufferId {
    pub backend: BackendId,
    pub target: TargetId,
}

impl BufferId {
    pub fn new(backend: BackendId, target: impl Into<TargetId>) -> Self {
        BufferId {
            backend,
            target: target.into(),
        }
    }

    /// The status buffer for a backend.
    pub fn status(backend: BackendId) -> Self {
        BufferId {
            backend,
            target: TargetId::status(),
        }
    }
}

/// Server-assigned id for a single event. Matrix exposes a stable event id;
/// IRC generally does not, hence the wrapping in `Option` at use sites. Required
/// to address edits and redactions.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventId(pub String);

/// Client-side correlation id for local echo: an outgoing message is sent with a
/// `TxnId`, and the backend's echo of it carries the same id so the optimistic
/// local copy can be replaced in place instead of duplicated. Generalizes the
/// IRCv3 labeled-response `label`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TxnId(pub u64);

/// Hands out monotonically increasing [`TxnId`]s for local-echo correlation.
/// Process-global; ids only need to be unique among in-flight outgoing messages.
#[derive(Debug, Default)]
pub struct TxnAllocator(AtomicU64);

impl TxnAllocator {
    pub const fn new() -> Self {
        TxnAllocator(AtomicU64::new(1))
    }

    pub fn next(&self) -> TxnId {
        TxnId(self.0.fetch_add(1, Ordering::Relaxed))
    }
}

/// A participant. `id` is stable (IRC nick, Matrix `@user:server`); `display` is
/// the mutable presentation name when it differs from the id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserRef {
    pub id: String,
    pub display: Option<String>,
}

impl UserRef {
    pub fn new(id: impl Into<String>) -> Self {
        UserRef {
            id: id.into(),
            display: None,
        }
    }

    pub fn with_display(id: impl Into<String>, display: impl Into<String>) -> Self {
        UserRef {
            id: id.into(),
            display: Some(display.into()),
        }
    }

    /// The name to show: the display name when set, otherwise the id.
    pub fn name(&self) -> &str {
        self.display.as_deref().unwrap_or(&self.id)
    }
}

/// Optional rich rendering of a message body alongside its plain text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Formatted {
    /// Matrix `formatted_body` (HTML subset).
    Html(String),
}

/// The broad category of a message attachment, used to pick an icon/label and to
/// decide whether the renderer attempts to display it inline (only `Image`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachmentKind {
    Image,
    Video,
    Audio,
    File,
}

impl AttachmentKind {
    /// Lowercase label used in the textual fallback line (`[image: name] url`).
    pub fn label(self) -> &'static str {
        match self {
            AttachmentKind::Image => "image",
            AttachmentKind::Video => "video",
            AttachmentKind::Audio => "audio",
            AttachmentKind::File => "file",
        }
    }
}

/// A media attachment on a message (image, file, ...). Carries a resolvable URL
/// for the textual fallback and, for images the backend has fetched, a local
/// cache path the renderer can display inline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub kind: AttachmentKind,
    /// Filename, alt text, or the message body describing the attachment.
    pub name: String,
    /// Resolvable http(s) URL, when one can be derived; the textual fallback.
    pub url: Option<String>,
    /// Protocol source reference (e.g. a Matrix `mxc://` uri) when no plain URL
    /// is available.
    pub source: Option<String>,
    pub mime: Option<String>,
    /// Local cache path of the downloaded bytes, set by the backend for images so
    /// the renderer can display them inline. Not exposed to Lua themes.
    pub local_path: Option<std::path::PathBuf>,
}

impl Attachment {
    /// A non-image attachment (file/video/audio) that is only ever shown as a
    /// textual fallback line.
    pub fn link(kind: AttachmentKind, name: impl Into<String>) -> Self {
        Attachment {
            kind,
            name: name.into(),
            url: None,
            source: None,
            mime: None,
            local_path: None,
        }
    }
}

/// A message body: always has a plain-text form, optionally a rich form and/or
/// media attachments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageBody {
    pub text: String,
    pub formatted: Option<Formatted>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

impl MessageBody {
    pub fn plain(text: impl Into<String>) -> Self {
        MessageBody {
            text: text.into(),
            formatted: None,
            attachments: Vec::new(),
        }
    }

    /// A body carrying media attachments alongside its (possibly empty) text.
    pub fn with_attachments(text: impl Into<String>, attachments: Vec<Attachment>) -> Self {
        MessageBody {
            text: text.into(),
            formatted: None,
            attachments,
        }
    }
}

/// How a message should be presented. `Action` is IRC CTCP ACTION / Matrix
/// `m.emote`; `Notice` is a lower-emphasis line (IRC NOTICE / Matrix `m.notice`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MsgKind {
    Text,
    Action,
    Notice,
}

/// A member's standing in a buffer, ordered most-privileged first so it doubles
/// as a sort key. Maps from IRC access levels (`@`/`+`/...) and, later, Matrix
/// power levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemberRole {
    Owner,
    Admin,
    Op,
    HalfOp,
    Voice,
    Member,
}

/// How a buffer should be classified, beyond its channel/DM target. Backends set
/// this to let the UI mark special buffers (e.g. a Matrix homeserver
/// server-notices room) distinctly from ordinary conversations. `Normal` is the
/// default; further variants (direct, space) can be added as detection lands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BufferKind {
    #[default]
    Normal,
    /// A homeserver/admin buffer such as a Matrix `m.server_notice` room.
    System,
}

/// A change to a single user's presence within one buffer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MembershipChange {
    /// Roster seeding (e.g. an IRC NAMES reply): the user is present, without the
    /// "has joined" announcement that [`Join`](MembershipChange::Join) implies.
    Present {
        role: MemberRole,
    },
    Join {
        /// IRC extended-join real name / gecos, when advertised. `None` for
        /// Matrix (which surfaces a display name via [`UserRef::display`]).
        realname: Option<String>,
    },
    Part {
        reason: Option<String>,
    },
    Kick {
        by: UserRef,
        reason: Option<String>,
    },
    Invite {
        by: UserRef,
    },
    /// The user's role changed (IRC MODE +o/+v, Matrix power level).
    SetRole {
        role: MemberRole,
    },
}

/// A normalized chat event. Every backend maps its protocol onto these variants;
/// anything that has no normalized form lands in [`ChatEvent::ServerInfo`] with
/// the raw line preserved for themes that want it.
///
/// Variants carry a `target` when they belong to a specific buffer. [`Rename`]
/// and [`Quit`] are identity-scoped: they affect every buffer the user is in and
/// so carry no target.
///
/// [`Rename`]: ChatEvent::Rename
/// [`Quit`]: ChatEvent::Quit
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatEvent {
    Message {
        target: TargetId,
        id: Option<EventId>,
        sender: UserRef,
        body: MessageBody,
        kind: MsgKind,
        /// Set on a server echo of our own outgoing message.
        echo_of: Option<TxnId>,
        /// Server-assigned timestamp (Matrix `origin_server_ts`, IRC `server-time`
        /// tag). `None` for an optimistic local echo, which is stamped with the
        /// local send time until the server's copy replaces it.
        time: Option<DateTime<Utc>>,
    },
    Edit {
        target: TargetId,
        id: EventId,
        body: MessageBody,
    },
    Redaction {
        target: TargetId,
        id: EventId,
        by: Option<UserRef>,
    },
    Reaction {
        target: TargetId,
        id: EventId,
        sender: UserRef,
        key: String,
        /// `true` to add the reaction, `false` to remove it.
        add: bool,
    },
    Membership {
        target: TargetId,
        who: UserRef,
        change: MembershipChange,
        /// Server-assigned timestamp, so a join/part line sorts at the time it
        /// actually happened rather than the moment it was received. `None` for
        /// roster seeding and protocols without a timestamp.
        time: Option<DateTime<Utc>>,
    },
    Topic {
        target: TargetId,
        who: Option<UserRef>,
        topic: String,
        /// Server-assigned timestamp; see [`Membership`](ChatEvent::Membership).
        time: Option<DateTime<Utc>>,
    },
    /// A user changed their display name / nick across all their buffers.
    Rename { who: UserRef, new_display: String },
    /// A user disconnected entirely, leaving all their buffers.
    Quit {
        who: UserRef,
        reason: Option<String>,
    },
    /// Sets a human-friendly display name for a buffer (e.g. a Matrix room name)
    /// without rendering a line. Creates the buffer if it does not exist yet, so
    /// backends can surface joined rooms proactively.
    BufferName { target: TargetId, name: String },
    /// Sets the topic on a buffer without rendering a chat line. Used during
    /// startup to restore already-known state from the backend's local store;
    /// actual topic changes use [`Topic`](ChatEvent::Topic) which does render a line.
    BufferTopic { target: TargetId, topic: String },
    /// Classifies a buffer (e.g. flags a Matrix server-notices room as
    /// [`System`](BufferKind::System)) without rendering a line. Creates the buffer
    /// if it does not exist yet, like [`BufferName`](ChatEvent::BufferName).
    BufferKind { target: TargetId, kind: BufferKind },
    /// Records whether the local user may post to a buffer (Matrix power levels),
    /// so the UI can hint that the room is read-only. Metadata only - renders no
    /// line. Creates the buffer if it does not exist yet.
    BufferPostPolicy { target: TargetId, can_post: bool },
    /// Server-originated or otherwise un-normalized line. `target` of `None`
    /// routes to the backend's status buffer. `from` is the originating server
    /// or nick, when known. `code` is a protocol-specific classifier (IRC numeric
    /// symbolic name like `RPL_WELCOME`, or a verb like `MODE`; later a Matrix
    /// state-event type) that themes can branch on or suppress. `raw` carries the
    /// wire representation for theme escape hatches.
    ServerInfo {
        target: Option<TargetId>,
        from: Option<String>,
        code: Option<String>,
        text: String,
        raw: Option<String>,
        /// Server-assigned timestamp, so a backfilled notice (e.g. a Matrix room
        /// state change replayed from history) sorts at the time it happened rather
        /// than when it was received. `None` for live lines, which are stamped with
        /// the local receive time.
        time: Option<DateTime<Utc>>,
    },
}

impl ChatEvent {
    /// A stand-in [`Message`](ChatEvent::Message) for message content a backend
    /// received but cannot render, so it surfaces as a line instead of being
    /// dropped. Attributed to its sender and stamped so it sorts in place; shown
    /// as a lower-emphasis notice.
    pub fn unsupported_message(
        target: TargetId,
        id: Option<EventId>,
        sender: UserRef,
        type_name: &str,
        time: Option<DateTime<Utc>>,
    ) -> Self {
        ChatEvent::Message {
            target,
            id,
            sender,
            body: MessageBody::plain(format!("[unsupported message of type {type_name}]")),
            kind: MsgKind::Notice,
            echo_of: None,
            time,
        }
    }

    /// A status-buffer line for a non-message event a backend received but does
    /// not map to a normalized variant, so protocol gaps surface instead of
    /// disappearing. `type_name` is preserved as the `code` for themes to branch on.
    pub fn unsupported_event(type_name: impl Into<String>) -> Self {
        let type_name = type_name.into();
        ChatEvent::ServerInfo {
            target: None,
            from: None,
            code: Some(type_name.clone()),
            text: format!("[unsupported event {type_name}]"),
            raw: None,
            time: None,
        }
    }

    /// The buffer target this event routes to, when it belongs to a specific
    /// buffer. `None` for identity-scoped events ([`Rename`](ChatEvent::Rename),
    /// [`Quit`](ChatEvent::Quit)) and status-bound [`ServerInfo`](ChatEvent::ServerInfo).
    pub fn target(&self) -> Option<&TargetId> {
        match self {
            ChatEvent::Message { target, .. }
            | ChatEvent::Edit { target, .. }
            | ChatEvent::Redaction { target, .. }
            | ChatEvent::Reaction { target, .. }
            | ChatEvent::Membership { target, .. }
            | ChatEvent::Topic { target, .. }
            | ChatEvent::BufferName { target, .. }
            | ChatEvent::BufferTopic { target, .. }
            | ChatEvent::BufferKind { target, .. }
            | ChatEvent::BufferPostPolicy { target, .. } => Some(target),
            ChatEvent::ServerInfo { target, .. } => target.as_ref(),
            ChatEvent::Rename { .. } | ChatEvent::Quit { .. } => None,
        }
    }

    /// Whether this event only mutates buffer metadata, with no chat line and no
    /// Lua representation. Such events must not be forwarded to the Lua `event`
    /// callback nor stored as a rendered message - `to_lua_event` treats them as
    /// [`unreachable!`]. Keep this in sync with the silent arms there.
    pub fn is_silent_state_update(&self) -> bool {
        matches!(
            self,
            ChatEvent::BufferTopic { .. }
                | ChatEvent::BufferKind { .. }
                | ChatEvent::BufferPostPolicy { .. }
        )
    }
}

/// Backend connection lifecycle and normalized events, as delivered to the core.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendEvent {
    /// The connection is up and identified under `nickname`.
    Ready {
        nickname: String,
    },
    /// All initial history has been delivered. Events after this are live.
    /// IRC sends this immediately after Ready (no backfill); Matrix sends it
    /// after the populate_room loop so backfill messages don't trigger
    /// unread/mention indicators.
    Synced,
    Disconnected {
        reason: Option<String>,
    },
    Error {
        message: String,
    },
    /// Round-trip time measured by the backend (IRC PING/PONG, Matrix whoami probe).
    Latency {
        ms: u64,
    },
    /// Answer to [`Command::FetchHistory`]: the fetch finished (successfully or
    /// not). `at_start` is true when no more history exists for `target` (or
    /// the backend/server cannot provide any), so the UI stops asking.
    HistoryFetched {
        target: TargetId,
        at_start: bool,
    },
    Event(ChatEvent),
}

/// One [`BackendEvent`] tagged with its originating backend, as carried on the
/// shared event channel the main loop drains.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendMessage {
    pub backend: BackendId,
    pub event: BackendEvent,
}

/// A protocol-agnostic outgoing action. The UI/Lua layer enqueues these; the
/// owning backend translates each into protocol-specific wire commands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    SendMessage {
        target: TargetId,
        body: String,
        kind: MsgKind,
        txn: TxnId,
    },
    Join {
        target: TargetId,
    },
    Part {
        target: TargetId,
        reason: Option<String>,
    },
    SetTopic {
        target: TargetId,
        topic: String,
    },
    React {
        target: TargetId,
        id: EventId,
        key: String,
        /// `true` to add the reaction, `false` to remove the local user's own.
        add: bool,
    },
    Redact {
        target: TargetId,
        id: EventId,
    },
    SetNick {
        nick: String,
    },
    Whois {
        user: String,
    },
    Kick {
        target: TargetId,
        user: String,
        reason: Option<String>,
    },
    Invite {
        user: String,
        target: TargetId,
    },
    Away {
        message: Option<String>,
    },
    ListChannels,
    /// Fetch a page of older messages for `target`. The UI supplies cursor
    /// hints from its oldest stored message; each backend uses what its
    /// protocol needs (IRC: `before` timestamp, Mattermost: `before_id`,
    /// Matrix: its own per-room pagination token). Every handling path must
    /// answer with [`BackendEvent::HistoryFetched`], even on error or lack of
    /// support, so the UI's in-flight marker always clears.
    FetchHistory {
        target: TargetId,
        /// Timestamp of the oldest stored message; `None` for an empty buffer
        /// (backends then fetch the latest page).
        before: Option<DateTime<Utc>>,
        /// Event id of the oldest stored message that has one.
        before_id: Option<EventId>,
        limit: u16,
    },
    /// Drives interactive device verification (Matrix SAS). IRC has no analogue
    /// and ignores it.
    Verify(VerifyAction),
    Quit {
        reason: Option<String>,
    },
}

/// A step in an interactive device-verification (SAS) exchange. The backend keeps
/// the in-flight verification; these actions advance it. Modelled as discrete
/// commands because verification is driven by user input arriving over time
/// (accept a request, then compare the short-auth-string, then confirm), not a
/// single round-trip.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerifyAction {
    /// Start verifying another user, or our own other devices when `user` is
    /// `None` (self-verification).
    Request { user: Option<String> },
    /// Accept the pending incoming verification request.
    Accept,
    /// Confirm that the displayed short-auth-string matches the other device.
    Confirm,
    /// Decline a pending request or abort the in-flight verification.
    Cancel,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_buffer_helpers() {
        let buffer = BufferId::status(BackendId(0));
        assert!(buffer.target.is_status());
        assert_eq!(buffer.target.as_str(), TargetId::STATUS);
    }

    #[test]
    fn silent_state_updates_are_not_forwarded_to_lua() {
        // These carry no chat line and hit `unreachable!` in to_lua_event, so
        // emit_lua_event must skip them. Regression guard for the BufferKind panic.
        let target = TargetId::from("!room:m.org");
        assert!(ChatEvent::BufferTopic {
            target: target.clone(),
            topic: "t".into(),
        }
        .is_silent_state_update());
        assert!(ChatEvent::BufferKind {
            target: target.clone(),
            kind: BufferKind::System,
        }
        .is_silent_state_update());
        assert!(ChatEvent::BufferPostPolicy {
            target: target.clone(),
            can_post: false,
        }
        .is_silent_state_update());

        // Events that do render / reach Lua must not be classified as silent.
        assert!(!ChatEvent::BufferName {
            target: target.clone(),
            name: "Room".into(),
        }
        .is_silent_state_update());
        assert!(!ChatEvent::ServerInfo {
            target: None,
            from: None,
            code: None,
            text: "hi".into(),
            raw: None,
            time: None,
        }
        .is_silent_state_update());
    }

    #[test]
    fn user_ref_name_prefers_display() {
        assert_eq!(UserRef::new("alice").name(), "alice");
        assert_eq!(UserRef::with_display("@a:m.org", "Alice").name(), "Alice");
    }

    #[test]
    fn txn_allocator_is_monotonic() {
        let alloc = TxnAllocator::new();
        let a = alloc.next();
        let b = alloc.next();
        assert_ne!(a, b);
        assert!(b.0 > a.0);
    }

    #[test]
    fn message_body_attachments_default_when_absent() {
        // Older serialized bodies have no `attachments` field; it must default to
        // empty rather than fail to deserialize.
        let body: MessageBody = serde_json::from_str(r#"{"text":"hi","formatted":null}"#).unwrap();
        assert_eq!(body.text, "hi");
        assert!(body.attachments.is_empty());
    }

    #[test]
    fn attachment_round_trips_through_serde() {
        let body = MessageBody::with_attachments(
            "look",
            vec![Attachment {
                kind: AttachmentKind::Image,
                name: "cat.png".to_string(),
                url: Some("https://hs/cat.png".to_string()),
                source: Some("mxc://hs/abc".to_string()),
                mime: Some("image/png".to_string()),
                local_path: None,
            }],
        );
        let json = serde_json::to_string(&body).unwrap();
        let back: MessageBody = serde_json::from_str(&json).unwrap();
        assert_eq!(body, back);
        assert_eq!(back.attachments[0].kind, AttachmentKind::Image);
    }

    #[test]
    fn unsupported_helpers_describe_the_type() {
        let msg = ChatEvent::unsupported_message(
            TargetId::from("#c"),
            None,
            UserRef::new("alice"),
            "m.location",
            None,
        );
        match msg {
            ChatEvent::Message { body, kind, .. } => {
                assert!(body.text.contains("unsupported message of type m.location"));
                assert_eq!(kind, MsgKind::Notice);
            }
            other => panic!("expected Message, got {other:?}"),
        }

        match ChatEvent::unsupported_event("m.receipt") {
            ChatEvent::ServerInfo { code, text, .. } => {
                assert_eq!(code.as_deref(), Some("m.receipt"));
                assert!(text.contains("unsupported event m.receipt"));
            }
            other => panic!("expected ServerInfo, got {other:?}"),
        }
    }

    #[test]
    fn buffer_id_is_usable_as_map_key() {
        use indexmap::IndexMap;

        let mut buffers: IndexMap<BufferId, u8> = IndexMap::new();
        buffers.insert(BufferId::new(BackendId(0), "#tirc"), 1);
        buffers.insert(BufferId::new(BackendId(1), "#tirc"), 2);

        // Same channel name on two backends must not collide.
        assert_eq!(buffers.len(), 2);
        assert_eq!(buffers[&BufferId::new(BackendId(0), "#tirc")], 1);
    }
}
