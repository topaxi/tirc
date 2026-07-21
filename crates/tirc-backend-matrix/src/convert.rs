//! Shared translation helpers: Matrix events and rooms into normalized
//! [`ChatEvent`]s, media handling, and small client-side lookups. Everything
//! here is driver-agnostic and reused by both sync drivers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use matrix_sdk::media::{MediaFormat, MediaRequestParameters};
use matrix_sdk::ruma::events::room::encrypted::OriginalSyncRoomEncryptedEvent;
use matrix_sdk::ruma::events::room::message::{
    MessageType, OriginalSyncRoomMessageEvent, Relation,
};
use matrix_sdk::ruma::events::room::MediaSource;
use matrix_sdk::ruma::events::tag::TagName;
use matrix_sdk::ruma::events::{
    AnySyncMessageLikeEvent, AnySyncStateEvent, AnySyncTimelineEvent, SyncStateEvent,
};
use matrix_sdk::ruma::{OwnedRoomId, RoomId, UserId};
use matrix_sdk::{Client, Room};

use tirc_core::backend::EventSender;
use tirc_core::{
    Attachment, AttachmentKind, BackendEvent, BackendId, BackendMessage, BufferKind, ChatEvent,
    EventId, Formatted, MemberRole, MembershipChange, MessageBody, MsgKind, TargetId, TxnId,
    UserRef,
};

pub(crate) fn trim_display_name(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{00A0}')
}

/// Converts a Matrix `origin_server_ts` into a UTC instant for chronological
/// ordering of the line it belongs to.
pub(crate) fn server_ts(
    ts: matrix_sdk::ruma::MilliSecondsSinceUnixEpoch,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let millis: u64 = ts.0.into();
    i64::try_from(millis)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
}

/// A line for the backend's status buffer (connection feedback, `:list`, ...).
pub(crate) fn status_line(text: String) -> ChatEvent {
    ChatEvent::ServerInfo {
        target: None,
        from: None,
        code: None,
        text,
        raw: None,
        time: None,
    }
}

/// Maps a Matrix power level to a member role (100 = admin, 50 = moderator).
/// Room creators have "infinite" power and map to owner.
pub(crate) fn role_from_power(
    power: matrix_sdk::ruma::events::room::power_levels::UserPowerLevel,
) -> MemberRole {
    use matrix_sdk::ruma::events::room::power_levels::UserPowerLevel;

    let value: i64 = match power {
        UserPowerLevel::Infinite => return MemberRole::Owner,
        UserPowerLevel::Int(int) => int.into(),
        _ => 0,
    };

    if value >= 100 {
        MemberRole::Owner
    } else if value >= 50 {
        MemberRole::Op
    } else {
        MemberRole::Member
    }
}

/// Whether the local user is allowed to post messages in `room`, per its power
/// levels. Absent or unreadable power levels are treated as postable, so a read
/// failure never wrongly locks the user out of a room they can actually write to.
pub(crate) async fn room_can_post(room: &Room) -> bool {
    match room.power_levels().await {
        Ok(power_levels) => power_levels.user_can_send_message(
            room.own_user_id(),
            matrix_sdk::ruma::events::MessageLikeEventType::RoomMessage,
        ),
        Err(_) => true,
    }
}

/// Whether we are already a joined member of `room` (a room id), so a join can
/// be skipped.
pub(crate) fn already_joined(client: &Client, room: &str) -> bool {
    RoomId::parse(room)
        .ok()
        .and_then(|room_id| client.get_room(&room_id))
        .map(|room| room.state() == matrix_sdk::RoomState::Joined)
        .unwrap_or(false)
}

pub(crate) async fn join(client: &Client, room: &str) -> anyhow::Result<()> {
    if let Ok(room_id) = RoomId::parse(room) {
        client.join_room_by_id(&room_id).await?;
    } else {
        let alias = matrix_sdk::ruma::RoomOrAliasId::parse(room)?;
        client.join_room_by_id_or_alias(&alias, &[]).await?;
    }
    Ok(())
}

pub(crate) fn room_by_target(client: &Client, target: &TargetId) -> Option<Room> {
    let room_id: OwnedRoomId = RoomId::parse(target.as_str()).ok()?;
    client.get_room(&room_id)
}

pub(crate) fn room_target(room: &Room) -> TargetId {
    TargetId(room.room_id().to_string())
}

/// Whether `room` is the homeserver's server-notices room, identified by the
/// `m.server_notice` room tag the homeserver sets on it.
pub(crate) async fn is_server_notice_room(room: &Room) -> bool {
    matches!(room.tags().await, Ok(Some(tags)) if tags.contains_key(&TagName::ServerNotice))
}

fn message_body(
    body: String,
    formatted: Option<matrix_sdk::ruma::events::room::message::FormattedBody>,
) -> MessageBody {
    MessageBody {
        text: body,
        formatted: formatted.map(|f| Formatted::Html(f.body)),
        attachments: Vec::new(),
    }
}

/// Media larger than this is still surfaced as a fallback line, but not
/// downloaded, so a huge upload cannot stall the backend or fill the cache.
const MAX_MEDIA_BYTES: usize = 10 * 1024 * 1024;

/// Whether a timeline event is already handled by a dedicated handler, so the
/// catch-all in the classic driver's handler registration must not also emit a
/// line for it.
pub(crate) fn is_handled_timeline_event(event: &AnySyncTimelineEvent) -> bool {
    matches!(
        event,
        AnySyncTimelineEvent::MessageLike(
            AnySyncMessageLikeEvent::RoomMessage(_)
                | AnySyncMessageLikeEvent::RoomEncrypted(_)
                | AnySyncMessageLikeEvent::Reaction(_)
                | AnySyncMessageLikeEvent::RoomRedaction(_)
        ) | AnySyncTimelineEvent::State(
            AnySyncStateEvent::RoomMember(_) | AnySyncStateEvent::RoomTopic(_)
        )
    )
}

/// A room-scoped line for an event we received but do not map to a normalized
/// variant, so protocol gaps surface in the room instead of vanishing. `time` is
/// the event's server timestamp, so a backfilled line sorts in place.
fn unsupported_room_event(
    room: &Room,
    type_name: String,
    time: Option<chrono::DateTime<chrono::Utc>>,
) -> ChatEvent {
    ChatEvent::ServerInfo {
        target: Some(room_target(room)),
        from: None,
        code: Some(type_name.clone()),
        text: format!("[unsupported event {type_name}]"),
        raw: None,
        time,
    }
}

/// A recognized room state change, extracted from its ruma event so the wording
/// in [`describe_room_state_change`] can be built (and unit-tested) without a
/// [`Room`] or a live homeserver.
pub(crate) enum RoomStateChange<'a> {
    Created,
    Renamed(&'a str),
    CanonicalAlias(Option<&'a str>),
    PowerLevels,
    JoinRule(&'a str),
    HistoryVisibility(&'a str),
    GuestAccess(&'a str),
    Avatar { removed: bool },
}

/// Human-readable notice text for a room state change, attributed to `actor`.
/// The wording mirrors Element's timeline strings for consistency; unrecognized
/// enum values (the content enums are `#[non_exhaustive]`) fall back to a plain
/// "set X to <value>" form using the raw value.
pub(crate) fn describe_room_state_change(actor: &str, change: &RoomStateChange) -> String {
    match change {
        RoomStateChange::Created => format!("{actor} created the room"),
        RoomStateChange::Renamed("") => format!("{actor} removed the room name"),
        RoomStateChange::Renamed(name) => {
            format!("{actor} changed the room name to {name}")
        }
        RoomStateChange::CanonicalAlias(None) => {
            format!("{actor} removed the main address for this room")
        }
        RoomStateChange::CanonicalAlias(Some(alias)) => {
            format!("{actor} set the main address for this room to {alias}")
        }
        RoomStateChange::PowerLevels => format!("{actor} changed the power levels"),
        RoomStateChange::JoinRule(rule) => match *rule {
            "invite" => format!("{actor} made the room invite only"),
            "public" => format!("{actor} made the room public to whoever knows the link"),
            "knock" => format!("{actor} allowed users to knock on the room"),
            other => format!("{actor} changed the join rule to {other}"),
        },
        RoomStateChange::HistoryVisibility(vis) => match *vis {
            "world_readable" => format!("{actor} made future room history visible to anyone"),
            "shared" => {
                format!("{actor} made future room history visible to all room members")
            }
            "invited" => format!(
                "{actor} made future room history visible to all room members, from the point they are invited"
            ),
            "joined" => format!(
                "{actor} made future room history visible to all room members, from the point they joined"
            ),
            other => format!("{actor} set history visibility to {other}"),
        },
        RoomStateChange::GuestAccess(access) => match *access {
            "can_join" => format!("{actor} has allowed guests to join the room"),
            "forbidden" => format!("{actor} has prevented guests from joining the room"),
            other => format!("{actor} set guest access to {other}"),
        },
        RoomStateChange::Avatar { removed: true } => format!("{actor} removed the room avatar"),
        RoomStateChange::Avatar { removed: false } => format!("{actor} changed the room avatar"),
    }
}

/// Extracts the event type and a [`RoomStateChange`] from a recognized room state
/// event, or `None` for events we do not describe (including redacted ones), which
/// then fall back to [`unsupported_room_event`]. The content enums are
/// `#[non_exhaustive]`, so their values are stringified via `as_str` rather than
/// matched arm-by-arm.
fn room_state_change(state: &AnySyncStateEvent) -> Option<(&'static str, RoomStateChange<'_>)> {
    match state {
        AnySyncStateEvent::RoomCreate(SyncStateEvent::Original(_)) => {
            Some(("m.room.create", RoomStateChange::Created))
        }
        AnySyncStateEvent::RoomName(SyncStateEvent::Original(event)) => {
            Some(("m.room.name", RoomStateChange::Renamed(&event.content.name)))
        }
        AnySyncStateEvent::RoomCanonicalAlias(SyncStateEvent::Original(event)) => Some((
            "m.room.canonical_alias",
            RoomStateChange::CanonicalAlias(event.content.alias.as_ref().map(|a| a.as_str())),
        )),
        AnySyncStateEvent::RoomPowerLevels(SyncStateEvent::Original(_)) => {
            Some(("m.room.power_levels", RoomStateChange::PowerLevels))
        }
        AnySyncStateEvent::RoomJoinRules(SyncStateEvent::Original(event)) => Some((
            "m.room.join_rules",
            RoomStateChange::JoinRule(event.content.join_rule.as_str()),
        )),
        AnySyncStateEvent::RoomHistoryVisibility(SyncStateEvent::Original(event)) => Some((
            "m.room.history_visibility",
            RoomStateChange::HistoryVisibility(event.content.history_visibility.as_str()),
        )),
        AnySyncStateEvent::RoomGuestAccess(SyncStateEvent::Original(event)) => Some((
            "m.room.guest_access",
            RoomStateChange::GuestAccess(event.content.guest_access.as_str()),
        )),
        AnySyncStateEvent::RoomAvatar(SyncStateEvent::Original(event)) => Some((
            "m.room.avatar",
            RoomStateChange::Avatar {
                removed: event.content.url.is_none(),
            },
        )),
        _ => None,
    }
}

/// Maps a timeline event to a room line: a descriptive notice for a recognized
/// room state change (attributed to the actor), otherwise the generic
/// [`unsupported_room_event`] fallback so unmapped events still surface. Shared by
/// the live catch-all handler and the backfill path.
pub(crate) async fn room_event_line(room: &Room, event: &AnySyncTimelineEvent) -> ChatEvent {
    let time = server_ts(event.origin_server_ts());
    if let AnySyncTimelineEvent::State(state) = event {
        if let Some((code, change)) = room_state_change(state) {
            let actor = sender_ref(room, state.sender()).await;
            let name = actor.display.unwrap_or(actor.id);
            return ChatEvent::ServerInfo {
                target: Some(room_target(room)),
                from: Some(name.clone()),
                code: Some(code.to_string()),
                text: describe_room_state_change(&name, &change),
                raw: None,
                time,
            };
        }
    }
    unsupported_room_event(room, event.event_type().to_string(), time)
}

/// Downloads a media source into the cache directory and returns its path. A raw
/// homeserver media URL is not browser-openable (authenticated media requires a
/// bearer token), so the SDK-authenticated download to a local file is what makes
/// the attachment reachable. Skips oversized media per [`MAX_MEDIA_BYTES`].
async fn download_media(
    room: &Room,
    source: &MediaSource,
    size: Option<usize>,
    ext: Option<&str>,
    media_dir: &Path,
) -> Option<PathBuf> {
    if size.is_some_and(|size| size > MAX_MEDIA_BYTES) {
        return None;
    }

    let path = media_dir.join(media_cache_key(source, ext));
    if path.exists() {
        return Some(path);
    }

    let request = MediaRequestParameters {
        source: source.clone(),
        format: MediaFormat::File,
    };
    match room
        .client()
        .media()
        .get_media_content(&request, true)
        .await
    {
        Ok(bytes) if bytes.len() <= MAX_MEDIA_BYTES => match tokio::fs::write(&path, bytes).await {
            Ok(()) => Some(path),
            Err(err) => {
                log::warn!("failed to cache media at {path:?}: {err}");
                None
            }
        },
        Ok(_) => None,
        Err(err) => {
            log::warn!("failed to download media: {err}");
            None
        }
    }
}

/// A filesystem-safe cache filename derived from the media's mxc id, with the
/// original file extension appended so the cached file opens in the right viewer.
fn media_cache_key(source: &MediaSource, ext: Option<&str>) -> String {
    let raw = match source {
        MediaSource::Plain(mxc) => mxc.to_string(),
        MediaSource::Encrypted(file) => file.url.to_string(),
    };
    let mut key: String = raw
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    if let Some(ext) = ext {
        key.push('.');
        key.push_str(ext);
    }
    key
}

/// Picks a file extension for the cache filename, preferring the one on the
/// original filename and falling back to the mime subtype (e.g. `image/png` ->
/// `png`).
fn media_extension(name: &str, mime: Option<&str>) -> Option<String> {
    if let Some(ext) = Path::new(name).extension().and_then(|e| e.to_str()) {
        return Some(ext.to_ascii_lowercase());
    }
    mime.and_then(|m| m.split('/').nth(1))
        .map(|sub| sub.to_ascii_lowercase())
}

/// Builds an [`Attachment`] for a media message, downloading the content into the
/// cache so it is reachable via its local path (and so images can render inline).
async fn media_attachment(
    room: &Room,
    kind: AttachmentKind,
    name: String,
    source: &MediaSource,
    mime: Option<String>,
    size: Option<usize>,
    media_dir: &Path,
) -> Attachment {
    let source_ref = match source {
        MediaSource::Plain(mxc) => mxc.to_string(),
        MediaSource::Encrypted(file) => file.url.to_string(),
    };

    let ext = media_extension(&name, mime.as_deref());
    let local_path = download_media(room, source, size, ext.as_deref(), media_dir).await;
    // The openable link is the cached file itself; a raw homeserver URL would
    // 401/404 in a browser without the access token.
    let url = local_path
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());

    Attachment {
        kind,
        name,
        url,
        source: Some(source_ref),
        mime,
        local_path,
    }
}

/// Translates a message payload into a body, surfacing media as attachments and
/// unknown message types as a placeholder line so nothing is silently dropped.
/// Returns the presentation kind alongside the body.
pub(crate) async fn msgtype_to_body(
    room: &Room,
    msgtype: MessageType,
    media_dir: &Path,
) -> (MsgKind, MessageBody) {
    match msgtype {
        MessageType::Text(content) => {
            (MsgKind::Text, message_body(content.body, content.formatted))
        }
        MessageType::Emote(content) => (
            MsgKind::Action,
            message_body(content.body, content.formatted),
        ),
        MessageType::Notice(content) => (
            MsgKind::Notice,
            message_body(content.body, content.formatted),
        ),
        MessageType::Image(content) => {
            let mime = content.info.as_ref().and_then(|i| i.mimetype.clone());
            let size = content
                .info
                .as_ref()
                .and_then(|i| i.size)
                .map(|s| u64::from(s) as usize);
            let caption = content.caption().unwrap_or_default().to_string();
            let attachment = media_attachment(
                room,
                AttachmentKind::Image,
                content.filename().to_string(),
                &content.source,
                mime,
                size,
                media_dir,
            )
            .await;
            (
                MsgKind::Text,
                MessageBody::with_attachments(caption, vec![attachment]),
            )
        }
        MessageType::File(content) => {
            let mime = content.info.as_ref().and_then(|i| i.mimetype.clone());
            let size = content
                .info
                .as_ref()
                .and_then(|i| i.size)
                .map(|s| u64::from(s) as usize);
            let caption = content.caption().unwrap_or_default().to_string();
            let attachment = media_attachment(
                room,
                AttachmentKind::File,
                content.filename().to_string(),
                &content.source,
                mime,
                size,
                media_dir,
            )
            .await;
            (
                MsgKind::Text,
                MessageBody::with_attachments(caption, vec![attachment]),
            )
        }
        MessageType::Video(content) => {
            let mime = content.info.as_ref().and_then(|i| i.mimetype.clone());
            let size = content
                .info
                .as_ref()
                .and_then(|i| i.size)
                .map(|s| u64::from(s) as usize);
            let caption = content.caption().unwrap_or_default().to_string();
            let attachment = media_attachment(
                room,
                AttachmentKind::Video,
                content.filename().to_string(),
                &content.source,
                mime,
                size,
                media_dir,
            )
            .await;
            (
                MsgKind::Text,
                MessageBody::with_attachments(caption, vec![attachment]),
            )
        }
        MessageType::Audio(content) => {
            let mime = content.info.as_ref().and_then(|i| i.mimetype.clone());
            let size = content
                .info
                .as_ref()
                .and_then(|i| i.size)
                .map(|s| u64::from(s) as usize);
            let caption = content.caption().unwrap_or_default().to_string();
            let attachment = media_attachment(
                room,
                AttachmentKind::Audio,
                content.filename().to_string(),
                &content.source,
                mime,
                size,
                media_dir,
            )
            .await;
            (
                MsgKind::Text,
                MessageBody::with_attachments(caption, vec![attachment]),
            )
        }
        MessageType::ServerNotice(content) => (MsgKind::Notice, MessageBody::plain(content.body)),
        other => (
            MsgKind::Notice,
            MessageBody::plain(format!("[unsupported message of type {}]", other.msgtype())),
        ),
    }
}

/// Translates a room message event into a normalized [`ChatEvent`]. Shared by the
/// live sync handler and history backfill so both render identically. `echo_of`
/// is recovered from the homeserver-echoed transaction id (the Matrix analogue of
/// IRC's labeled-response), so our own sends de-duplicate against their optimistic
/// local copy in `State`. Never drops a message: unknown types
/// surface as a placeholder line.
pub(crate) async fn message_event_to_chat(
    event: OriginalSyncRoomMessageEvent,
    room: &Room,
    media_dir: &Path,
) -> ChatEvent {
    // An m.replace relation means this is an edit of an earlier event.
    if let Some(Relation::Replacement(replacement)) = event.content.relates_to {
        let (_, body) = msgtype_to_body(room, replacement.new_content.msgtype, media_dir).await;
        return ChatEvent::Edit {
            target: room_target(room),
            id: EventId(replacement.event_id.to_string()),
            body,
        };
    }

    let (kind, body) = msgtype_to_body(room, event.content.msgtype, media_dir).await;

    let echo_of = event
        .unsigned
        .transaction_id
        .as_ref()
        .and_then(|txn| txn.as_str().parse::<u64>().ok())
        .map(TxnId);

    let time = server_ts(event.origin_server_ts);

    ChatEvent::Message {
        target: room_target(room),
        id: Some(EventId(event.event_id.to_string())),
        sender: sender_ref(room, &event.sender).await,
        body,
        kind,
        echo_of,
        time,
    }
}

/// Like [`sender_ref`], but for the local user's own events returns a `UserRef`
/// keyed by the bare localpart so it matches the registered nickname (which is
/// the localpart, not the full MXID). This lets `State` mark the reaction as
/// `mine`.
pub(crate) async fn own_or_sender_ref(room: &Room, user: &UserId, mine: bool) -> UserRef {
    if mine {
        UserRef::new(room.own_user_id().localpart())
    } else {
        sender_ref(room, user).await
    }
}

/// Queries the homeserver's public room directory and reports it into the status
/// buffer, the Matrix analogue of IRC's `/list`.
pub(crate) async fn list_public_rooms(client: &Client, id: BackendId, events: &EventSender) {
    let response = match client.public_rooms(Some(50), None, None).await {
        Ok(response) => response,
        Err(err) => {
            emit(events, id, status_line(format!("LIST failed: {err}")));
            return;
        }
    };

    emit(
        events,
        id,
        status_line(format!("{} public room(s):", response.chunk.len())),
    );

    for room in response.chunk {
        let handle = room
            .canonical_alias
            .map(|alias| alias.to_string())
            .unwrap_or_else(|| room.room_id.to_string());
        let name = room.name.unwrap_or_default();
        emit(
            events,
            id,
            status_line(format!(
                "{handle}  {name}  ({} members)",
                room.num_joined_members
            )),
        );
    }
}

/// Placeholder line for an event we hold but cannot decrypt (missing the Megolm
/// session). Carries the real sender, id and timestamp so it sorts in place and
/// is attributed correctly; only the body stands in for the ciphertext.
pub(crate) async fn utd_placeholder(
    room: &Room,
    event: &OriginalSyncRoomEncryptedEvent,
) -> ChatEvent {
    ChatEvent::Message {
        target: room_target(room),
        id: Some(EventId(event.event_id.to_string())),
        sender: sender_ref(room, &event.sender).await,
        body: MessageBody::plain("[unable to decrypt message - encryption keys unavailable]"),
        kind: MsgKind::Text,
        echo_of: None,
        time: server_ts(event.origin_server_ts),
    }
}

/// Reports the device's encryption posture into the status buffer at startup so
/// the user can tell whether this session can decrypt traffic, and whether it
/// still needs verifying from another client.
pub(crate) async fn report_crypto_status(client: &Client, id: BackendId, events: &EventSender) {
    let encryption = client.encryption();

    let device_line = match encryption.get_own_device().await {
        Ok(Some(device)) => {
            let state = if device.is_verified() {
                "verified"
            } else {
                "unverified"
            };
            format!("Encryption: device {} ({state})", device.device_id())
        }
        Ok(None) => "Encryption: own device missing from the crypto store".to_string(),
        Err(err) => format!("Encryption: could not query own device: {err}"),
    };
    emit(events, id, status_line(device_line));

    let cross_signing = match encryption.cross_signing_status().await {
        Some(status) if status.is_complete() => "Cross-signing: set up".to_string(),
        Some(_) => {
            "Cross-signing: incomplete; verify this session from another client to receive keys"
                .to_string()
        }
        None => {
            "Cross-signing: not set up; messages restricted to verified devices will not decrypt \
             until this session is verified"
                .to_string()
        }
    };
    emit(events, id, status_line(cross_signing));
}

/// Resolves a sender into a [`UserRef`], using the room-local display name when
/// available (no network round-trip).
pub(crate) async fn sender_ref(room: &Room, user: &UserId) -> UserRef {
    let display = room
        .get_member_no_sync(user)
        .await
        .ok()
        .flatten()
        .and_then(|member| {
            member
                .display_name()
                .map(trim_display_name)
                .map(str::to_string)
        });

    UserRef {
        id: user.to_string(),
        display,
    }
}

/// Reads the persisted map of room-id -> last-shown topic. Returns an empty map
/// when the file is absent (first run) or unreadable.
pub(crate) fn load_known_topics(path: &std::path::Path) -> HashMap<String, String> {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(_) => return HashMap::new(),
    };
    match serde_json::from_str(&data) {
        Ok(map) => map,
        Err(err) => {
            log::warn!("ignoring unreadable known-topics file at {path:?}: {err}");
            HashMap::new()
        }
    }
}

/// Persists the known-topics map so the next run can compare and suppress
/// unchanged topics. Best-effort: a write failure only causes one extra
/// topic line on the next startup.
pub(crate) fn save_known_topics(path: &std::path::Path, topics: &HashMap<String, String>) {
    match serde_json::to_string(topics) {
        Ok(data) => {
            if let Err(err) = std::fs::write(path, data) {
                log::warn!("failed to persist known topics to {path:?}: {err}");
            }
        }
        Err(err) => log::warn!("failed to serialize known topics: {err}"),
    }
}

pub(crate) fn emit(events: &EventSender, backend: BackendId, event: ChatEvent) {
    let _ = events.send(BackendMessage {
        backend,
        event: BackendEvent::Event(event),
    });
}

/// Emits the buffer-metadata lines for a room - its name, post policy,
/// server-notice kind, topic, and joined-member roster - so a joined room is
/// visible with its roster without waiting for new activity. Shared by both
/// drivers. Topic de-duplication uses `known_topics`: an unchanged topic is
/// sent as a passive `BufferTopic`, a changed one as a `Topic` line (and
/// recorded so the next run can suppress it).
pub(crate) async fn emit_room_metadata(
    room: &Room,
    id: BackendId,
    events: &EventSender,
    known_topics: &mut HashMap<String, String>,
) {
    let target = room_target(room);

    let name = room
        .display_name()
        .await
        .map(|name| name.to_string())
        .unwrap_or_else(|_| target.0.clone());
    emit(
        events,
        id,
        ChatEvent::BufferName {
            target: target.clone(),
            name,
        },
    );

    emit(
        events,
        id,
        ChatEvent::BufferPostPolicy {
            target: target.clone(),
            can_post: room_can_post(room).await,
        },
    );

    // The homeserver's server-notices room (matrix.org surfaces this as the
    // "Official Account") is tagged `m.server_notice`. Flag it so the UI can mark
    // it distinctly, akin to an IRC server window, while it stays a real room.
    if is_server_notice_room(room).await {
        emit(
            events,
            id,
            ChatEvent::BufferKind {
                target: target.clone(),
                kind: BufferKind::System,
            },
        );
    }

    if let Some(topic) = room.topic() {
        let room_id = room.room_id().to_string();
        if known_topics.get(&room_id).map(String::as_str) == Some(topic.as_str()) {
            emit(
                events,
                id,
                ChatEvent::BufferTopic {
                    target: target.clone(),
                    topic,
                },
            );
        } else {
            known_topics.insert(room_id, topic.clone());
            emit(
                events,
                id,
                ChatEvent::Topic {
                    target: target.clone(),
                    who: None,
                    topic,
                    time: None,
                },
            );
        }
    }

    if let Ok(members) = room.members(matrix_sdk::RoomMemberships::JOIN).await {
        for member in members {
            emit(
                events,
                id,
                ChatEvent::Membership {
                    target: target.clone(),
                    who: UserRef {
                        id: member.user_id().to_string(),
                        display: member
                            .display_name()
                            .map(trim_display_name)
                            .map(str::to_string),
                    },
                    change: MembershipChange::Present {
                        role: role_from_power(member.power_level()),
                    },
                    time: None,
                },
            );
        }
    }
}

/// Spawns the periodic round-trip probe: calls `whoami()` every 30s and emits
/// the RTT as a [`BackendEvent::Latency`]. After 3 consecutive failures it emits
/// [`BackendEvent::Disconnected`] once so the buffer bar shows an offline
/// indicator (the SDK retries its sync internally, so no explicit disconnect
/// arrives otherwise), and a visible "Connection restored" line once the probe
/// recovers. Shared by both drivers. The returned handle should be aborted on
/// shutdown.
pub(crate) fn spawn_latency_probe(
    client: Client,
    id: BackendId,
    events: EventSender,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(30);
        let mut failures: u32 = 0;
        let mut degraded = false;
        loop {
            tokio::time::sleep(interval).await;
            let start = std::time::Instant::now();
            if client.whoami().await.is_ok() {
                failures = 0;
                if degraded {
                    degraded = false;
                    emit(&events, id, status_line("Connection restored".to_string()));
                }
                let ms = start.elapsed().as_millis() as u64;
                let _ = events.send(BackendMessage {
                    backend: id,
                    event: BackendEvent::Latency { ms },
                });
            } else {
                failures += 1;
                if failures >= 3 && !degraded {
                    degraded = true;
                    let _ = events.send(BackendMessage {
                        backend: id,
                        event: BackendEvent::Disconnected { reason: None },
                    });
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_extension_prefers_filename_then_mime() {
        assert_eq!(media_extension("cat.PNG", None).as_deref(), Some("png"));
        assert_eq!(
            media_extension("report", Some("application/pdf")).as_deref(),
            Some("pdf")
        );
        assert_eq!(media_extension("noext", None), None);
    }

    #[test]
    fn describe_room_state_change_wording() {
        let describe = |change| describe_room_state_change("alice", &change);

        assert_eq!(describe(RoomStateChange::Created), "alice created the room");
        assert_eq!(
            describe(RoomStateChange::Renamed("Lounge")),
            "alice changed the room name to Lounge"
        );
        assert_eq!(
            describe(RoomStateChange::Renamed("")),
            "alice removed the room name"
        );
        assert_eq!(
            describe(RoomStateChange::CanonicalAlias(Some("#lounge:example.org"))),
            "alice set the main address for this room to #lounge:example.org"
        );
        assert_eq!(
            describe(RoomStateChange::CanonicalAlias(None)),
            "alice removed the main address for this room"
        );
        assert_eq!(
            describe(RoomStateChange::PowerLevels),
            "alice changed the power levels"
        );
        assert_eq!(
            describe(RoomStateChange::JoinRule("invite")),
            "alice made the room invite only"
        );
        assert_eq!(
            describe(RoomStateChange::JoinRule("public")),
            "alice made the room public to whoever knows the link"
        );
        assert_eq!(
            describe(RoomStateChange::JoinRule("restricted")),
            "alice changed the join rule to restricted"
        );
        assert_eq!(
            describe(RoomStateChange::HistoryVisibility("shared")),
            "alice made future room history visible to all room members"
        );
        assert_eq!(
            describe(RoomStateChange::GuestAccess("can_join")),
            "alice has allowed guests to join the room"
        );
        assert_eq!(
            describe(RoomStateChange::GuestAccess("forbidden")),
            "alice has prevented guests from joining the room"
        );
        assert_eq!(
            describe(RoomStateChange::Avatar { removed: false }),
            "alice changed the room avatar"
        );
        assert_eq!(
            describe(RoomStateChange::Avatar { removed: true }),
            "alice removed the room avatar"
        );
    }
}
