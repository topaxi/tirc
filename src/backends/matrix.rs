//! Matrix backend: the sole place that touches `matrix-sdk`.
//!
//! Unencrypted rooms only for now (E2E is a deferred follow-up). The SDK's sync
//! loop drives incoming events through registered handlers that translate Matrix
//! timeline/state events into normalized [`ChatEvent`]s; outgoing [`Command`]s
//! are applied directly to the client.

use matrix_sdk::config::SyncSettings;
use matrix_sdk::room::MessagesOptions;
use matrix_sdk::ruma::events::room::member::MembershipState;
use matrix_sdk::ruma::events::room::member::SyncRoomMemberEvent;
use matrix_sdk::ruma::events::room::message::{
    MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent, SyncRoomMessageEvent,
};
use matrix_sdk::ruma::events::room::topic::SyncRoomTopicEvent;
use matrix_sdk::ruma::events::{
    AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent,
};
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::ruma::{OwnedRoomId, OwnedTransactionId, RoomId, UserId};
use matrix_sdk::{Client, Room};

use crate::core::{
    BackendEvent, BackendId, BackendMessage, ChatEvent, Command, EventId, Formatted, MemberRole,
    MembershipChange, MessageBody, MsgKind, Protocol, TargetId, TxnId, UserRef,
};

use super::{BackendInfo, ChatBackend, CommandReceiver, EventSender};

/// Connection parameters for a Matrix backend, built from the user config.
#[derive(Clone, Debug)]
pub struct MatrixBackendConfig {
    pub homeserver: String,
    pub user_id: String,
    pub password: String,
    pub device_id: Option<String>,
    pub autojoin: Vec<String>,
    /// Override for the SQLite store directory. Defaults to an XDG data path
    /// derived from the user id when `None`.
    pub store_dir: Option<std::path::PathBuf>,
}

pub struct MatrixBackend {
    id: BackendId,
    config: MatrixBackendConfig,
}

impl MatrixBackend {
    pub fn new(id: BackendId, config: MatrixBackendConfig) -> Self {
        MatrixBackend { id, config }
    }

    /// Per-account SQLite store directory, so sync tokens and (later) crypto
    /// state persist across runs.
    fn store_path(&self) -> anyhow::Result<std::path::PathBuf> {
        if let Some(dir) = &self.config.store_dir {
            std::fs::create_dir_all(dir)?;
            return Ok(dir.clone());
        }

        let sanitized: String = self
            .config
            .user_id
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();

        Ok(xdg::BaseDirectories::with_prefix("tirc")
            .create_data_directory(format!("matrix/{sanitized}"))?)
    }

    /// Path of the persisted auth session, alongside the SQLite store. The SDK
    /// store keeps sync tokens and crypto state but not the login session, so we
    /// serialize the `MatrixSession` ourselves and restore it next run instead of
    /// re-running a password login (which mints a brand-new device every time).
    fn session_path(store_path: &std::path::Path) -> std::path::PathBuf {
        store_path.join("session.json")
    }
}

#[async_trait::async_trait]
impl ChatBackend for MatrixBackend {
    fn info(&self) -> BackendInfo {
        BackendInfo {
            id: self.id,
            protocol: Protocol::Matrix,
            name: self.config.homeserver.clone(),
        }
    }

    async fn run(
        self: Box<Self>,
        events: EventSender,
        mut commands: CommandReceiver,
    ) -> anyhow::Result<()> {
        let id = self.id;
        let store_path = self.store_path()?;
        let session_path = Self::session_path(&store_path);

        let client = authenticate(&self.config, &store_path, &session_path).await?;

        let user_id = client
            .user_id()
            .map(|user| user.as_str().to_string())
            .unwrap_or_else(|| self.config.user_id.clone());
        let nickname = client
            .user_id()
            .map(|user| user.localpart().to_string())
            .unwrap_or_else(|| self.config.user_id.clone());
        let _ = events.send(BackendMessage {
            backend: id,
            event: BackendEvent::Ready {
                nickname: nickname.clone(),
            },
        });
        // Unlike IRC there are no server numerics, so emit explicit connection
        // feedback into the status buffer.
        emit(&events, id, status_line(format!("Logged in as {user_id}")));

        // Initial sync loads current room state into the store and advances the
        // store's sync token. Handlers are registered *after* it, so pre-existing
        // members don't generate spurious "has joined" lines; we surface joined
        // rooms explicitly instead.
        let _ = client.sync_once(SyncSettings::default()).await;

        let joined = client.joined_rooms();
        emit(
            &events,
            id,
            status_line(format!("Synced; {} joined room(s)", joined.len())),
        );
        for room in joined {
            populate_room(&room, id, &events).await;
        }

        register_handlers(&client, id, events.clone());

        // Drive the SDK sync loop in the background; it resumes from the store's
        // token (set by sync_once) so it delivers only new events.
        let sync_client = client.clone();
        let sync = tokio::spawn(async move {
            let _ = sync_client.sync(SyncSettings::default()).await;
        });

        // Autojoin configured rooms (aliases or ids), skipping ones we are
        // already in. Re-joining wastes a round-trip and some homeservers even
        // return 5xx for it, which the SDK retries with backoff and would stall
        // command processing.
        for room in &self.config.autojoin {
            if already_joined(&client, room) {
                continue;
            }
            let _ = join(&client, room).await;
        }

        while let Some(command) = commands.recv().await {
            apply_command(&client, id, &events, command).await;
        }

        sync.abort();
        Ok(())
    }
}

fn trim_display_name(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{00A0}')
}

/// Converts a Matrix `origin_server_ts` into a UTC instant for chronological
/// ordering of the line it belongs to.
fn server_ts(
    ts: matrix_sdk::ruma::MilliSecondsSinceUnixEpoch,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let millis: u64 = ts.0.into();
    i64::try_from(millis)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
}

/// Registers sync handlers translating Matrix events into [`ChatEvent`]s.
fn register_handlers(client: &Client, id: BackendId, events: EventSender) {
    let message_events = events.clone();
    client.add_event_handler(move |event: SyncRoomMessageEvent, room: Room| {
        let events = message_events.clone();
        async move {
            if let SyncRoomMessageEvent::Original(event) = event {
                if let Some(chat) = message_event_to_chat(event, &room).await {
                    emit(&events, id, chat);
                }
            }
        }
    });

    let member_events = events.clone();
    client.add_event_handler(move |event: SyncRoomMemberEvent, room: Room| {
        let events = member_events.clone();
        async move {
            if let SyncRoomMemberEvent::Original(event) = event {
                let was_joined = event
                    .unsigned
                    .prev_content
                    .as_ref()
                    .map(|prev| prev.membership == MembershipState::Join)
                    .unwrap_or(false);

                let change = match event.content.membership {
                    // A Join with a prior Join is a profile change, not an arrival.
                    MembershipState::Join if was_joined => return,
                    MembershipState::Join => MembershipChange::Join { realname: None },
                    MembershipState::Leave => MembershipChange::Part {
                        reason: event.content.reason.clone(),
                    },
                    MembershipState::Invite => MembershipChange::Invite {
                        by: UserRef::new(event.sender.to_string()),
                    },
                    _ => return,
                };

                let who = UserRef {
                    id: event.state_key.to_string(),
                    display: event
                        .content
                        .displayname
                        .as_deref()
                        .map(trim_display_name)
                        .map(str::to_string),
                };

                emit(
                    &events,
                    id,
                    ChatEvent::Membership {
                        target: room_target(&room),
                        who,
                        change,
                        time: server_ts(event.origin_server_ts),
                    },
                );
            }
        }
    });

    let topic_events = events;
    client.add_event_handler(move |event: SyncRoomTopicEvent, room: Room| {
        let events = topic_events.clone();
        async move {
            if let SyncRoomTopicEvent::Original(event) = event {
                emit(
                    &events,
                    id,
                    ChatEvent::Topic {
                        target: room_target(&room),
                        who: Some(UserRef::new(event.sender.to_string())),
                        topic: event.content.topic,
                        time: server_ts(event.origin_server_ts),
                    },
                );
            }
        }
    });
}

/// Applies an outgoing command to the Matrix client.
async fn apply_command(client: &Client, id: BackendId, events: &EventSender, command: Command) {
    match command {
        Command::SendMessage {
            target,
            body,
            kind,
            txn,
        } => {
            let Some(room) = room_by_target(client, &target) else {
                return;
            };

            // Optimistic local echo for perceived latency, mirroring the IRC path:
            // emit the message immediately tagged with `txn`, then send with the
            // same id as the Matrix transaction id. The homeserver echoes that id
            // back in the synced event's `unsigned.transaction_id`, so the sync
            // copy replaces this optimistic one in `State` instead of duplicating.
            let sender = client
                .user_id()
                .map(|user| UserRef::new(user.as_str()))
                .unwrap_or_else(|| UserRef::new("me"));
            emit(
                events,
                id,
                ChatEvent::Message {
                    target,
                    id: None,
                    sender,
                    body: MessageBody::plain(body.clone()),
                    kind,
                    echo_of: Some(txn),
                    time: None,
                },
            );

            let content = match kind {
                MsgKind::Action => RoomMessageEventContent::emote_plain(body),
                MsgKind::Notice => RoomMessageEventContent::notice_plain(body),
                _ => RoomMessageEventContent::text_plain(body),
            };
            let transaction_id = OwnedTransactionId::from(txn.0.to_string());
            let _ = room.send(content).with_transaction_id(transaction_id).await;
        }
        Command::Join { target } => {
            let _ = join(client, target.as_str()).await;
        }
        Command::Part { target, .. } => {
            if let Some(room) = room_by_target(client, &target) {
                let _ = room.leave().await;
            }
        }
        Command::SetTopic { target, topic } => {
            if let Some(room) = room_by_target(client, &target) {
                let _ = room.set_room_topic(&topic).await;
            }
        }
        Command::ListChannels => list_public_rooms(client, id, events).await,
        // Reactions/redactions and IRC-only commands are not handled yet.
        _ => {}
    }
}

/// Queries the homeserver's public room directory and reports it into the status
/// buffer, the Matrix analogue of IRC's `/list`.
async fn list_public_rooms(client: &Client, id: BackendId, events: &EventSender) {
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

/// A line for the backend's status buffer (connection feedback, `:list`, ...).
fn status_line(text: String) -> ChatEvent {
    ChatEvent::ServerInfo {
        target: None,
        from: None,
        code: None,
        text,
        raw: None,
    }
}

/// Surfaces an already-joined room as a named buffer with its topic and roster,
/// so joined rooms are visible on startup without waiting for new activity.
async fn populate_room(room: &Room, id: BackendId, events: &EventSender) {
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

    if let Some(topic) = room.topic() {
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

    backfill_room(room, id, events).await;
}

/// Maps a Matrix power level to a member role (100 = admin, 50 = moderator).
/// Room creators have "infinite" power and map to owner.
fn role_from_power(
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

/// Whether we are already a joined member of `room` (a room id), so a join can
/// be skipped.
fn already_joined(client: &Client, room: &str) -> bool {
    RoomId::parse(room)
        .ok()
        .and_then(|room_id| client.get_room(&room_id))
        .map(|room| room.state() == matrix_sdk::RoomState::Joined)
        .unwrap_or(false)
}

async fn join(client: &Client, room: &str) -> anyhow::Result<()> {
    if let Ok(room_id) = RoomId::parse(room) {
        client.join_room_by_id(&room_id).await?;
    } else {
        let alias = matrix_sdk::ruma::RoomOrAliasId::parse(room)?;
        client.join_room_by_id_or_alias(&alias, &[]).await?;
    }
    Ok(())
}

fn room_by_target(client: &Client, target: &TargetId) -> Option<Room> {
    let room_id: OwnedRoomId = RoomId::parse(target.as_str()).ok()?;
    client.get_room(&room_id)
}

fn room_target(room: &Room) -> TargetId {
    TargetId(room.room_id().to_string())
}

fn message_body(
    body: String,
    formatted: Option<matrix_sdk::ruma::events::room::message::FormattedBody>,
) -> MessageBody {
    MessageBody {
        text: body,
        formatted: formatted.map(|f| Formatted::Html(f.body)),
    }
}

/// Translates a room message event into a normalized [`ChatEvent::Message`].
/// Shared by the live sync handler and history backfill so both render
/// identically. `echo_of` is recovered from the homeserver-echoed transaction id
/// (the Matrix analogue of IRC's labeled-response), so our own sends de-duplicate
/// against their optimistic local copy in [`State`](crate::ui::State).
async fn message_event_to_chat(
    event: OriginalSyncRoomMessageEvent,
    room: &Room,
) -> Option<ChatEvent> {
    let (kind, body) = match event.content.msgtype {
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
        _ => return None,
    };

    let echo_of = event
        .unsigned
        .transaction_id
        .as_ref()
        .and_then(|txn| txn.as_str().parse::<u64>().ok())
        .map(TxnId);

    let time = server_ts(event.origin_server_ts);

    Some(ChatEvent::Message {
        target: room_target(room),
        id: Some(EventId(event.event_id.to_string())),
        sender: sender_ref(room, &event.sender).await,
        body,
        kind,
        echo_of,
        time,
    })
}

/// Backfills the most recent messages of a room (oldest-first) so freshly-opened
/// buffers show history instead of being empty until new activity.
async fn backfill_room(room: &Room, id: BackendId, events: &EventSender) {
    let mut options = MessagesOptions::backward();
    options.limit = 30u32.into();

    let Ok(messages) = room.messages(options).await else {
        return;
    };

    // `chunk` is newest-first; collect translated messages then emit oldest-first.
    let mut chats = Vec::new();
    for timeline_event in messages.chunk {
        if let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncMessageLikeEvent::Original(event),
        ))) = timeline_event.raw().deserialize()
        {
            if let Some(chat) = message_event_to_chat(event, room).await {
                chats.push(chat);
            }
        }
    }

    for chat in chats.into_iter().rev() {
        emit(events, id, chat);
    }
}

/// Resolves a sender into a [`UserRef`], using the room-local display name when
/// available (no network round-trip).
async fn sender_ref(room: &Room, user: &UserId) -> UserRef {
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

/// Builds and authenticates the client, reusing a persisted session when
/// possible so we do not register a new device on every connect. A restored
/// session is validated with a `whoami` round-trip; if the device was signed out
/// remotely the stale session is discarded and we fall back to a password login.
///
/// A fresh client is built for the fallback because `restore_session` and
/// `login` both panic if a session was already set on the same client.
async fn authenticate(
    config: &MatrixBackendConfig,
    store_path: &std::path::Path,
    session_path: &std::path::Path,
) -> anyhow::Result<Client> {
    let build_client = || async {
        Client::builder()
            .homeserver_url(&config.homeserver)
            .sqlite_store(store_path, None)
            .build()
            .await
    };

    if let Some(session) = load_session(session_path) {
        let client = build_client().await?;
        client.restore_session(session).await?;
        match client.whoami().await {
            Ok(_) => return Ok(client),
            Err(err) => {
                log::warn!("persisted matrix session is no longer valid ({err}); logging in again");
                let _ = std::fs::remove_file(session_path);
            }
        }
    }

    let client = build_client().await?;
    let mut login = client
        .matrix_auth()
        .login_username(&config.user_id, &config.password)
        .initial_device_display_name("tirc");
    if let Some(device_id) = &config.device_id {
        login = login.device_id(device_id);
    }
    login.await?;

    if let Some(session) = client.matrix_auth().session() {
        save_session(session_path, &session);
    }

    Ok(client)
}

/// Reads a previously persisted [`MatrixSession`], returning `None` when there is
/// no saved session or it cannot be parsed (in which case we fall back to a fresh
/// password login).
fn load_session(path: &std::path::Path) -> Option<MatrixSession> {
    let data = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&data) {
        Ok(session) => Some(session),
        Err(err) => {
            log::warn!("ignoring unreadable matrix session at {path:?}: {err}");
            None
        }
    }
}

/// Persists the login session so the next run restores it instead of logging in
/// again. Best-effort: a write failure only means we log in afresh next time.
fn save_session(path: &std::path::Path, session: &MatrixSession) {
    match serde_json::to_string(session) {
        Ok(data) => {
            if let Err(err) = std::fs::write(path, data) {
                log::warn!("failed to persist matrix session to {path:?}: {err}");
            }
        }
        Err(err) => log::warn!("failed to serialize matrix session: {err}"),
    }
}

fn emit(events: &EventSender, backend: BackendId, event: ChatEvent) {
    let _ = events.send(BackendMessage {
        backend,
        event: BackendEvent::Event(event),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::TxnId;
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// End-to-end check against a live homeserver (see `dev/matrix`). Logs in,
    /// joins a room, sends a message, and asserts it comes back through sync as a
    /// normalized event. Ignored by default since it needs the homeserver:
    ///
    /// ```sh
    /// TIRC_TEST_HOMESERVER=http://localhost:6167 \
    /// TIRC_TEST_USER=@alice:localhost TIRC_TEST_PASSWORD=alicepassword \
    /// TIRC_TEST_ROOM='!roomid' \
    ///   cargo test --lib matrix::tests -- --ignored --nocapture
    /// ```
    ///
    /// `TIRC_TEST_ROOM` must be the exact room id returned by `createRoom` -
    /// modern room versions (e.g. Conduit's default) use server-less ids with no
    /// `:server` suffix, and an over-qualified id will not resolve.
    #[tokio::test]
    #[ignore = "requires the local matrix homeserver from dev/matrix"]
    async fn login_join_send_roundtrip() {
        let config = MatrixBackendConfig {
            homeserver: std::env::var("TIRC_TEST_HOMESERVER").unwrap(),
            user_id: std::env::var("TIRC_TEST_USER").unwrap(),
            password: std::env::var("TIRC_TEST_PASSWORD").unwrap(),
            device_id: None,
            autojoin: vec![std::env::var("TIRC_TEST_ROOM").unwrap()],
            store_dir: Some(unique_store_dir()),
        };
        let room = config.autojoin[0].clone();

        let backend = Box::new(MatrixBackend::new(BackendId(0), config));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(backend.run(event_tx, command_rx));

        assert!(
            wait_for(&mut event_rx, |m| matches!(
                m.event,
                BackendEvent::Ready { .. }
            ))
            .await,
            "expected a Ready event after login"
        );

        command_tx
            .send(Command::SendMessage {
                target: TargetId(room),
                body: "hello from tirc".to_string(),
                kind: MsgKind::Text,
                txn: TxnId(1),
            })
            .unwrap();

        assert!(
            wait_for(&mut event_rx, |m| matches!(
                &m.event,
                BackendEvent::Event(ChatEvent::Message { body, .. }) if body.text == "hello from tirc"
            ))
            .await,
            "expected the sent message echoed back through sync"
        );

        drop(command_tx);
        let _ = handle.await;
    }

    /// Diagnoses startup behaviour: logs in and asserts that an already-joined
    /// room is surfaced as a named buffer (BufferName) within a few seconds.
    #[tokio::test]
    #[ignore = "requires the local matrix homeserver from dev/matrix"]
    async fn startup_surfaces_joined_rooms() {
        let config = MatrixBackendConfig {
            homeserver: std::env::var("TIRC_TEST_HOMESERVER").unwrap(),
            user_id: std::env::var("TIRC_TEST_USER").unwrap(),
            password: std::env::var("TIRC_TEST_PASSWORD").unwrap(),
            device_id: None,
            autojoin: vec![],
            store_dir: Some(unique_store_dir()),
        };

        let backend = Box::new(MatrixBackend::new(BackendId(0), config));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (_command_tx, command_rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(backend.run(event_tx, command_rx));

        let got_buffer_name = wait_for(&mut event_rx, |m| {
            matches!(&m.event, BackendEvent::Event(ChatEvent::BufferName { .. }))
        })
        .await;

        handle.abort();
        assert!(
            got_buffer_name,
            "expected a BufferName event surfacing a joined room on startup"
        );
    }

    /// A unique, throwaway store directory so concurrent/sequential test runs do
    /// not share sqlite state.
    fn unique_store_dir() -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("tirc-test-matrix-{nanos}"))
    }

    async fn wait_for(
        rx: &mut mpsc::UnboundedReceiver<BackendMessage>,
        pred: impl Fn(&BackendMessage) -> bool,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while let Ok(Some(message)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            if pred(&message) {
                return true;
            }
        }
        false
    }
}
