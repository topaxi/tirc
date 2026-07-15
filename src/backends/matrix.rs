//! Matrix backend: the sole place that touches `matrix-sdk`.
//!
//! E2E encryption is enabled: the crypto state (Olm/Megolm keys) is persisted in
//! the same per-account sqlite store as the sync token and login session, so a
//! restored session keeps its keys across runs. Encryption is otherwise
//! transparent - `room.send` auto-encrypts in encrypted rooms and the SDK
//! auto-decrypts incoming events during sync. The pieces that need explicit
//! handling are history backfill (the low-level `room.messages` API does not
//! decrypt) and events we lack the keys for, which surface as `[unable to
//! decrypt]` placeholders.
//!
//! Interactive (SAS) device verification is driven from the status buffer: an
//! incoming request is held pending until the user runs `:verify accept`, the
//! emoji short-auth-string is printed for comparison, and `:verify confirm` /
//! `:verify cancel` complete or abort it. The in-flight verification lives in a
//! shared [`Verifications`] handle because the request arrives on a sync handler
//! while the user's accept/confirm commands arrive on the command loop.
//!
//! The SDK's sync loop drives incoming events through registered handlers that
//! translate Matrix timeline/state events into normalized [`ChatEvent`]s;
//! outgoing [`Command`]s are applied directly to the client.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::Mutex;

use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::deserialized_responses::TimelineEvent;
use matrix_sdk::encryption::verification::{
    SasState, SasVerification, Verification, VerificationRequest, VerificationRequestState,
};
use matrix_sdk::media::{MediaFormat, MediaRequestParameters};
use matrix_sdk::room::MessagesOptions;
use matrix_sdk::ruma::events::key::verification::request::ToDeviceKeyVerificationRequestEvent;
use matrix_sdk::ruma::events::reaction::ReactionEventContent;
use matrix_sdk::ruma::events::relation::Annotation;
use matrix_sdk::ruma::events::room::encrypted::{
    OriginalSyncRoomEncryptedEvent, SyncRoomEncryptedEvent,
};
use matrix_sdk::ruma::events::room::member::MembershipState;
use matrix_sdk::ruma::events::room::member::SyncRoomMemberEvent;
use matrix_sdk::ruma::events::room::message::{
    MessageType, OriginalSyncRoomMessageEvent, Relation, RoomMessageEventContent,
    SyncRoomMessageEvent,
};
use matrix_sdk::ruma::events::room::redaction::SyncRoomRedactionEvent;
use matrix_sdk::ruma::events::room::topic::SyncRoomTopicEvent;
use matrix_sdk::ruma::events::room::MediaSource;
use matrix_sdk::ruma::events::tag::TagName;
use matrix_sdk::ruma::events::{
    AnySyncMessageLikeEvent, AnySyncStateEvent, AnySyncTimelineEvent, SyncMessageLikeEvent,
};
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::ruma::{OwnedRoomId, OwnedTransactionId, RoomId, UserId};
use matrix_sdk::{Client, Room};

use crate::core::{
    Attachment, AttachmentKind, BackendEvent, BackendId, BackendMessage, BufferKind, ChatEvent,
    Command, EventId, Formatted, MemberRole, MembershipChange, MessageBody, MsgKind, Protocol,
    TargetId, TxnId, UserRef, VerifyAction,
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
        // Cache directory for downloaded media (images shown inline). Best-effort:
        // a failure here only means images are not fetched, not that the backend
        // stops.
        let media_dir = store_path.join("media");
        let _ = std::fs::create_dir_all(&media_dir);

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
        report_crypto_status(&client, id, &events).await;

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

        let known_topics_path = store_path.join("known_topics.json");
        let mut known_topics = load_known_topics(&known_topics_path);
        for room in joined {
            populate_room(&room, id, &events, &mut known_topics, &media_dir).await;
        }
        save_known_topics(&known_topics_path, &known_topics);

        let _ = events.send(BackendMessage {
            backend: id,
            event: BackendEvent::Synced,
        });

        let known_topics = Arc::new(Mutex::new(known_topics));
        let verifications = Verifications::default();
        let reactions = ReactionIndex::default();
        register_handlers(
            &client,
            id,
            events.clone(),
            verifications.clone(),
            reactions.clone(),
            known_topics,
            known_topics_path,
            media_dir.clone(),
        );

        // Drive the SDK sync loop in the background; it resumes from the store's
        // token (set by sync_once) so it delivers only new events.
        let sync_client = client.clone();
        let sync = tokio::spawn(async move {
            let _ = sync_client.sync(SyncSettings::default()).await;
        });

        // Periodic round-trip probe: call whoami() every 30s and emit the RTT
        // as a Latency event. After 3 consecutive failures, emit Disconnected so
        // the buffer bar shows an offline indicator (the SDK retries sync internally
        // so we won't get an explicit Disconnected otherwise).
        let ping_client = client.clone();
        let ping_events = events.clone();
        let ping_task = tokio::spawn(async move {
            let mut interval = std::time::Duration::from_secs(30);
            let mut failures: u32 = 0;
            loop {
                tokio::time::sleep(interval).await;
                interval = std::time::Duration::from_secs(30);
                let start = std::time::Instant::now();
                if ping_client.whoami().await.is_ok() {
                    failures = 0;
                    let ms = start.elapsed().as_millis() as u64;
                    let _ = ping_events.send(BackendMessage {
                        backend: id,
                        event: BackendEvent::Latency { ms },
                    });
                } else {
                    failures += 1;
                    if failures >= 3 {
                        let _ = ping_events.send(BackendMessage {
                            backend: id,
                            event: BackendEvent::Disconnected { reason: None },
                        });
                    }
                }
            }
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
            apply_command(&client, id, &events, &verifications, &reactions, command).await;
        }

        sync.abort();
        ping_task.abort();
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

/// Tracks reaction events so a reaction redaction can be resolved back to the
/// message and key it targeted, and so the local user's own reaction can be
/// redacted when toggled off. Matrix models "unreact" as redacting the reaction
/// event, which requires knowing that event's id.
#[derive(Clone, Default)]
struct ReactionIndex {
    inner: Arc<Mutex<ReactionIndexState>>,
}

#[derive(Default)]
struct ReactionIndexState {
    /// reaction event id -> (target message id, key).
    by_event: HashMap<String, (String, String)>,
    /// (target message id, key) -> the local user's own reaction event id.
    mine: HashMap<(String, String), String>,
}

impl ReactionIndex {
    async fn record(&self, reaction_event: &str, target_id: &str, key: &str, mine: bool) {
        let mut state = self.inner.lock().await;
        state.by_event.insert(
            reaction_event.to_string(),
            (target_id.to_string(), key.to_string()),
        );
        if mine {
            state.mine.insert(
                (target_id.to_string(), key.to_string()),
                reaction_event.to_string(),
            );
        }
    }

    /// Resolves a redacted event id to the `(target, key)` it reacted to and
    /// forgets it. Returns `None` when the redaction was not for a tracked
    /// reaction (e.g. a normal message deletion).
    async fn resolve_redaction(&self, reaction_event: &str) -> Option<(String, String)> {
        let mut state = self.inner.lock().await;
        let (target, key) = state.by_event.remove(reaction_event)?;
        state.mine.remove(&(target.clone(), key.clone()));
        Some((target, key))
    }

    /// Removes and returns the local user's own reaction event id for a
    /// `(target, key)`, used to redact it when toggling the reaction off.
    async fn take_mine(&self, target_id: &str, key: &str) -> Option<String> {
        let mut state = self.inner.lock().await;
        state.mine.remove(&(target_id.to_string(), key.to_string()))
    }
}

/// Registers sync handlers translating Matrix events into [`ChatEvent`]s.
#[allow(clippy::too_many_arguments)]
fn register_handlers(
    client: &Client,
    id: BackendId,
    events: EventSender,
    verifications: Verifications,
    reactions: ReactionIndex,
    known_topics: Arc<Mutex<HashMap<String, String>>>,
    known_topics_path: PathBuf,
    media_dir: PathBuf,
) {
    let message_events = events.clone();
    let message_media_dir = media_dir.clone();
    client.add_event_handler(move |event: SyncRoomMessageEvent, room: Room| {
        let events = message_events.clone();
        let media_dir = message_media_dir.clone();
        async move {
            if let SyncRoomMessageEvent::Original(event) = event {
                emit(
                    &events,
                    id,
                    message_event_to_chat(event, &room, &media_dir).await,
                );
            }
        }
    });

    // Catch-all for timeline events not handled by a specific handler above, so
    // an unmapped event surfaces as a line instead of being dropped. This fires
    // for every timeline event (in addition to the specific handlers), hence the
    // explicit skip-list of variants already handled. Ephemeral events (typing,
    // receipts) are not timeline events and never reach this path.
    let catch_all_events = events.clone();
    client.add_event_handler(move |event: AnySyncTimelineEvent, room: Room| {
        let events = catch_all_events.clone();
        async move {
            if is_handled_timeline_event(&event) {
                return;
            }
            emit(
                &events,
                id,
                unsupported_room_event(&room, event.event_type().to_string()),
            );
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

    let topic_events = events.clone();
    client.add_event_handler(move |event: SyncRoomTopicEvent, room: Room| {
        let events = topic_events.clone();
        let known_topics = known_topics.clone();
        let known_topics_path = known_topics_path.clone();
        async move {
            if let SyncRoomTopicEvent::Original(event) = event {
                let topic = event.content.topic.clone();
                emit(
                    &events,
                    id,
                    ChatEvent::Topic {
                        target: room_target(&room),
                        who: Some(UserRef::new(event.sender.to_string())),
                        topic: topic.clone(),
                        time: server_ts(event.origin_server_ts),
                    },
                );
                let mut map = known_topics.lock().await;
                map.insert(room.room_id().to_string(), topic);
                save_known_topics(&known_topics_path, &map);
            }
        }
    });

    let redaction_events = events.clone();
    let redaction_index = reactions.clone();
    client.add_event_handler(move |event: SyncRoomRedactionEvent, room: Room| {
        let events = redaction_events.clone();
        let reactions = redaction_index.clone();
        async move {
            if let SyncRoomRedactionEvent::Original(event) = event {
                // `redacts` is at the event level in old room versions, inside
                // `content` in room version 11+. Try both.
                let redacted_id = event
                    .redacts
                    .as_deref()
                    .or(event.content.redacts.as_deref());
                if let Some(redacted_id) = redacted_id {
                    let mine = room.own_user_id().as_str() == event.sender.as_str();
                    // A redaction of a tracked reaction removes it; anything else
                    // is a normal message deletion.
                    if let Some((target, key)) =
                        reactions.resolve_redaction(redacted_id.as_str()).await
                    {
                        emit(
                            &events,
                            id,
                            ChatEvent::Reaction {
                                target: room_target(&room),
                                id: EventId(target),
                                sender: own_or_sender_ref(&room, &event.sender, mine).await,
                                key,
                                add: false,
                            },
                        );
                    } else {
                        emit(
                            &events,
                            id,
                            ChatEvent::Redaction {
                                target: room_target(&room),
                                id: EventId(redacted_id.to_string()),
                                by: Some(sender_ref(&room, &event.sender).await),
                            },
                        );
                    }
                }
            }
        }
    });

    let reaction_events = events.clone();
    let reaction_index = reactions.clone();
    client.add_event_handler(
        move |event: SyncMessageLikeEvent<ReactionEventContent>, room: Room| {
            let events = reaction_events.clone();
            let reactions = reaction_index.clone();
            async move {
                if let SyncMessageLikeEvent::Original(event) = event {
                    let target_id = event.content.relates_to.event_id.to_string();
                    let key = event.content.relates_to.key.clone();
                    let mine = room.own_user_id().as_str() == event.sender.as_str();
                    reactions
                        .record(event.event_id.as_str(), &target_id, &key, mine)
                        .await;
                    emit(
                        &events,
                        id,
                        ChatEvent::Reaction {
                            target: room_target(&room),
                            id: EventId(target_id),
                            sender: own_or_sender_ref(&room, &event.sender, mine).await,
                            key,
                            add: true,
                        },
                    );
                }
            }
        },
    );

    // Events the SDK successfully decrypts are re-dispatched under their inner
    // type (e.g. `m.room.message`), so they reach the message handler above. An
    // event still typed `m.room.encrypted` here is one we lack the keys for;
    // surface a placeholder rather than dropping it silently.
    let encrypted_events = events.clone();
    client.add_event_handler(move |event: SyncRoomEncryptedEvent, room: Room| {
        let events = encrypted_events.clone();
        async move {
            if let SyncRoomEncryptedEvent::Original(event) = event {
                emit(&events, id, utd_placeholder(&room, &event).await);
            }
        }
    });

    // An incoming verification request is held pending so the user can vet it
    // before it starts exchanging keys. We surface who asked and how to proceed;
    // the request is fetched by its transaction id and stashed for `:verify
    // accept` / `:verify cancel`.
    let verification_events = events;
    client.add_event_handler(
        move |event: ToDeviceKeyVerificationRequestEvent, client: Client| {
            let events = verification_events.clone();
            let verifications = verifications.clone();
            async move {
                let Some(request) = client
                    .encryption()
                    .get_verification_request(&event.sender, &event.content.transaction_id)
                    .await
                else {
                    return;
                };

                verifications.set_pending(request).await;
                emit(
                    &events,
                    id,
                    status_line(format!(
                        "Device verification requested by {} (device {}). \
                         Type :verify accept to compare the emoji, or :verify cancel to reject.",
                        event.sender, event.content.from_device
                    )),
                );
            }
        },
    );
}

/// Applies an outgoing command to the Matrix client.
async fn apply_command(
    client: &Client,
    id: BackendId,
    events: &EventSender,
    verifications: &Verifications,
    reactions: &ReactionIndex,
    command: Command,
) {
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
        Command::Verify(action) => apply_verify(client, id, events, verifications, action).await,
        Command::React {
            target,
            id: event_id,
            key,
            add,
        } => {
            let Some(room) = room_by_target(client, &target) else {
                return;
            };
            if add {
                let target_event = match matrix_sdk::ruma::EventId::parse(&event_id.0) {
                    Ok(event) => event,
                    Err(err) => {
                        log::warn!("React: invalid event id {}: {err}", event_id.0);
                        return;
                    }
                };
                let content = ReactionEventContent::new(Annotation::new(target_event, key.clone()));
                match room.send(content).await {
                    Ok(resp) => {
                        reactions
                            .record(resp.response.event_id.as_str(), &event_id.0, &key, true)
                            .await;
                    }
                    Err(err) => log::warn!("React add failed: {err}"),
                }
            } else if let Some(reaction_event) = reactions.take_mine(&event_id.0, &key).await {
                match matrix_sdk::ruma::EventId::parse(&reaction_event) {
                    Ok(reaction_event) => {
                        let _ = room.redact(&reaction_event, None, None).await;
                    }
                    Err(err) => log::warn!("React remove: invalid reaction id: {err}"),
                }
            } else {
                log::warn!("React remove: no known reaction to redact");
            }
        }
        // IRC-only commands are not handled here.
        _ => {}
    }
}

/// Like [`sender_ref`], but for the local user's own events returns a `UserRef`
/// keyed by the bare localpart so it matches the registered nickname (which is
/// the localpart, not the full MXID). This lets `State` mark the reaction as
/// `mine`.
async fn own_or_sender_ref(room: &Room, user: &UserId, mine: bool) -> UserRef {
    if mine {
        UserRef::new(room.own_user_id().localpart())
    } else {
        sender_ref(room, user).await
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

/// Shared in-flight verification state. The incoming-request handler and the
/// command loop run on different tasks, so the pending request and the active
/// SAS flow are held behind a mutex both can reach. Both fields are cheap SDK
/// handles, so cloning out of the lock to act on them is fine.
#[derive(Clone, Default)]
struct Verifications {
    /// An incoming request awaiting `:verify accept`.
    pending: Arc<Mutex<Option<VerificationRequest>>>,
    /// The SAS flow currently presenting an emoji string, for `:verify confirm`
    /// / `:verify cancel`.
    sas: Arc<Mutex<Option<SasVerification>>>,
}

impl Verifications {
    async fn set_pending(&self, request: VerificationRequest) {
        *self.pending.lock().await = Some(request);
    }

    async fn take_pending(&self) -> Option<VerificationRequest> {
        self.pending.lock().await.take()
    }

    async fn set_sas(&self, sas: SasVerification) {
        *self.sas.lock().await = Some(sas);
    }

    async fn clear_sas(&self) {
        *self.sas.lock().await = None;
    }

    async fn current_sas(&self) -> Option<SasVerification> {
        self.sas.lock().await.clone()
    }
}

/// Handles a `:verify ...` command. Accept/confirm/cancel act on the in-flight
/// verification; a bare `:verify [user]` initiates one.
async fn apply_verify(
    client: &Client,
    id: BackendId,
    events: &EventSender,
    verifications: &Verifications,
    action: VerifyAction,
) {
    match action {
        VerifyAction::Request { user } => {
            start_verification(client, id, events, verifications, user).await
        }
        VerifyAction::Accept => accept_verification(id, events, verifications).await,
        VerifyAction::Confirm => confirm_verification(id, events, verifications).await,
        VerifyAction::Cancel => cancel_verification(id, events, verifications).await,
    }
}

/// Initiates verification of `user` (or our own identity for self-verification
/// when `None`), then drives the resulting request to completion.
async fn start_verification(
    client: &Client,
    id: BackendId,
    events: &EventSender,
    verifications: &Verifications,
    user: Option<String>,
) {
    let user_id = match user {
        Some(user) => match UserId::parse(&user) {
            Ok(user_id) => user_id,
            Err(err) => {
                emit(
                    events,
                    id,
                    status_line(format!("Invalid user id {user:?}: {err}")),
                );
                return;
            }
        },
        None => match client.user_id() {
            Some(user_id) => user_id.to_owned(),
            None => return,
        },
    };

    let identity = match client.encryption().get_user_identity(&user_id).await {
        Ok(Some(identity)) => identity,
        Ok(None) => {
            emit(
                events,
                id,
                status_line(format!(
                    "Cannot verify {user_id}: no cross-signing identity (they have not set up \
                     verification)."
                )),
            );
            return;
        }
        Err(err) => {
            emit(
                events,
                id,
                status_line(format!("Verification lookup failed: {err}")),
            );
            return;
        }
    };

    match identity.request_verification().await {
        Ok(request) => {
            emit(
                events,
                id,
                status_line(format!(
                    "Verification request sent to {user_id}; waiting for them to accept."
                )),
            );
            spawn_drive_request(id, events.clone(), verifications.clone(), request);
        }
        Err(err) => emit(
            events,
            id,
            status_line(format!("Could not request verification: {err}")),
        ),
    }
}

/// Accepts the pending incoming request and drives it forward.
async fn accept_verification(id: BackendId, events: &EventSender, verifications: &Verifications) {
    let Some(request) = verifications.take_pending().await else {
        emit(
            events,
            id,
            status_line("No pending verification request to accept.".to_string()),
        );
        return;
    };

    if let Err(err) = request.accept().await {
        emit(
            events,
            id,
            status_line(format!("Failed to accept verification: {err}")),
        );
        return;
    }

    emit(
        events,
        id,
        status_line("Verification accepted; waiting for the emoji to compare...".to_string()),
    );
    spawn_drive_request(id, events.clone(), verifications.clone(), request);
}

/// Confirms the displayed short-auth-string matches, telling the SDK our device
/// trusts the other one.
async fn confirm_verification(id: BackendId, events: &EventSender, verifications: &Verifications) {
    let Some(sas) = verifications.current_sas().await else {
        emit(
            events,
            id,
            status_line("No verification in progress to confirm.".to_string()),
        );
        return;
    };

    match sas.confirm().await {
        Ok(()) => emit(
            events,
            id,
            status_line(
                "Marked the emoji as matching; waiting for the other device...".to_string(),
            ),
        ),
        Err(err) => emit(
            events,
            id,
            status_line(format!("Failed to confirm verification: {err}")),
        ),
    }
}

/// Cancels the in-flight SAS, or rejects a still-pending request when no SAS has
/// started yet.
async fn cancel_verification(id: BackendId, events: &EventSender, verifications: &Verifications) {
    if let Some(sas) = verifications.current_sas().await {
        let _ = sas.cancel().await;
        verifications.clear_sas().await;
        emit(
            events,
            id,
            status_line("Verification cancelled.".to_string()),
        );
        return;
    }

    if let Some(request) = verifications.take_pending().await {
        let _ = request.cancel().await;
        emit(
            events,
            id,
            status_line("Verification request rejected.".to_string()),
        );
        return;
    }

    emit(
        events,
        id,
        status_line("No verification to cancel.".to_string()),
    );
}

/// Spawns the background task that follows a verification request through its
/// state changes until it transitions into a SAS flow (then hands off to
/// [`drive_sas`]) or terminates.
fn spawn_drive_request(
    id: BackendId,
    events: EventSender,
    verifications: Verifications,
    request: VerificationRequest,
) {
    tokio::spawn(async move {
        drive_request(id, &events, &verifications, request).await;
    });
}

/// Follows a verification request's state stream. The side that initiated the
/// request starts the SAS once both are ready; the other side waits for the
/// resulting transition. Either way we end up driving the SAS emoji exchange.
async fn drive_request(
    id: BackendId,
    events: &EventSender,
    verifications: &Verifications,
    request: VerificationRequest,
) {
    let mut stream = request.changes();
    while let Some(state) = stream.next().await {
        match state {
            VerificationRequestState::Ready { .. } if request.we_started() => {
                match request.start_sas().await {
                    Ok(Some(sas)) => {
                        drive_sas(id, events, verifications, sas).await;
                        return;
                    }
                    Ok(None) => {}
                    Err(err) => {
                        emit(
                            events,
                            id,
                            status_line(format!("Could not start verification: {err}")),
                        );
                        return;
                    }
                }
            }
            VerificationRequestState::Transitioned {
                verification: Verification::SasV1(sas),
            } => {
                drive_sas(id, events, verifications, sas).await;
                return;
            }
            VerificationRequestState::Cancelled(info) => {
                emit(
                    events,
                    id,
                    status_line(format!("Verification cancelled: {}", info.reason())),
                );
                return;
            }
            VerificationRequestState::Done => return,
            _ => {}
        }
    }
}

/// Drives a SAS flow: accepts it (unless we started it), publishes the emoji
/// short-auth-string for the user to compare, and reports the outcome. The SAS
/// is stashed in [`Verifications`] so `:verify confirm` / `:verify cancel` can
/// act on it while this loop awaits the next state change.
async fn drive_sas(
    id: BackendId,
    events: &EventSender,
    verifications: &Verifications,
    sas: SasVerification,
) {
    if !sas.we_started() {
        if let Err(err) = sas.accept().await {
            emit(
                events,
                id,
                status_line(format!("Failed to start emoji verification: {err}")),
            );
            return;
        }
    }

    verifications.set_sas(sas.clone()).await;

    let mut stream = sas.changes();
    while let Some(state) = stream.next().await {
        match state {
            SasState::KeysExchanged { emojis, decimals } => {
                if let Some(emojis) = emojis {
                    emit(
                        events,
                        id,
                        status_line(format!("Compare emoji: {}", format_emojis(&emojis.emojis))),
                    );
                } else {
                    let (a, b, c) = decimals;
                    emit(
                        events,
                        id,
                        status_line(format!("Compare numbers: {a} {b} {c}")),
                    );
                }
                emit(
                    events,
                    id,
                    status_line(
                        "If they match the other device, run :verify confirm; otherwise :verify cancel."
                            .to_string(),
                    ),
                );
            }
            SasState::Done { .. } => {
                emit(
                    events,
                    id,
                    status_line("Device verified successfully.".to_string()),
                );
                break;
            }
            SasState::Cancelled(info) => {
                emit(
                    events,
                    id,
                    status_line(format!("Verification cancelled: {}", info.reason())),
                );
                break;
            }
            _ => {}
        }
    }

    verifications.clear_sas().await;
}

/// Formats the seven SAS emojis as `symbol description` pairs on one line.
fn format_emojis(emojis: &[matrix_sdk::encryption::verification::Emoji; 7]) -> String {
    emojis
        .iter()
        .map(|emoji| format!("{} {}", emoji.symbol, emoji.description))
        .collect::<Vec<_>>()
        .join("   ")
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
async fn populate_room(
    room: &Room,
    id: BackendId,
    events: &EventSender,
    known_topics: &mut HashMap<String, String>,
    media_dir: &Path,
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

    backfill_room(room, id, events, media_dir).await;
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

/// Whether `room` is the homeserver's server-notices room, identified by the
/// `m.server_notice` room tag the homeserver sets on it.
async fn is_server_notice_room(room: &Room) -> bool {
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
/// catch-all in [`register_handlers`] must not also emit a line for it.
fn is_handled_timeline_event(event: &AnySyncTimelineEvent) -> bool {
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
/// variant, so protocol gaps surface in the room instead of vanishing.
fn unsupported_room_event(room: &Room, type_name: String) -> ChatEvent {
    ChatEvent::ServerInfo {
        target: Some(room_target(room)),
        from: None,
        code: Some(type_name.clone()),
        text: format!("[unsupported event {type_name}]"),
        raw: None,
    }
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
async fn msgtype_to_body(
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
/// local copy in [`State`](crate::ui::State). Never drops a message: unknown types
/// surface as a placeholder line.
async fn message_event_to_chat(
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

/// Backfills the most recent messages of a room (oldest-first) so freshly-opened
/// buffers show history instead of being empty until new activity.
async fn backfill_room(room: &Room, id: BackendId, events: &EventSender, media_dir: &Path) {
    let mut options = MessagesOptions::backward();
    options.limit = 30u32.into();

    let Ok(messages) = room.messages(options).await else {
        return;
    };

    // `chunk` is newest-first; collect translated messages then emit oldest-first.
    let mut chats = Vec::new();
    for timeline_event in messages.chunk {
        if let Some(chat) = backfill_event_to_chat(timeline_event, room, media_dir).await {
            chats.push(chat);
        }
    }

    for chat in chats.into_iter().rev() {
        emit(events, id, chat);
    }
}

/// Translates a single backfilled timeline event into a [`ChatEvent`]. Unlike
/// the live sync path, `room.messages` returns raw events without decrypting, so
/// encrypted ones are decrypted here (falling back to a placeholder when the keys
/// are unavailable). Message-like events that map to nothing else surface as an
/// unsupported-event line rather than being dropped.
async fn backfill_event_to_chat(
    timeline_event: TimelineEvent,
    room: &Room,
    media_dir: &Path,
) -> Option<ChatEvent> {
    let raw = timeline_event.raw();
    let deserialized = raw.deserialize();
    match deserialized {
        Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncMessageLikeEvent::Original(event),
        ))) => Some(message_event_to_chat(event, room, media_dir).await),
        Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomEncrypted(
            SyncMessageLikeEvent::Original(event),
        ))) => Some(decrypt_backfill_event(event, raw, room, media_dir).await),
        Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomRedaction(
            SyncRoomRedactionEvent::Original(event),
        ))) => {
            let redacted_id = event
                .redacts
                .as_deref()
                .or(event.content.redacts.as_deref())?;
            Some(ChatEvent::Redaction {
                target: room_target(room),
                id: EventId(redacted_id.to_string()),
                by: Some(sender_ref(room, &event.sender).await),
            })
        }
        Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::Reaction(
            SyncMessageLikeEvent::Original(event),
        ))) => Some(ChatEvent::Reaction {
            target: room_target(room),
            id: EventId(event.content.relates_to.event_id.to_string()),
            sender: sender_ref(room, &event.sender).await,
            key: event.content.relates_to.key,
            add: true,
        }),
        // Events handled elsewhere (membership/topic seeded by populate_room) are
        // skipped; anything else surfaces as an unsupported-event line so a gap is
        // visible rather than silent.
        Ok(other) if is_handled_timeline_event(&other) => None,
        Ok(other) => Some(unsupported_room_event(room, other.event_type().to_string())),
        Err(_) => None,
    }
}

/// Attempts to decrypt a backfilled `m.room.encrypted` event, returning the inner
/// message or a placeholder when we cannot decrypt it. The raw event is cast back
/// to its encrypted form (already known from deserialization) for the SDK's
/// store-backed decryption.
async fn decrypt_backfill_event(
    encrypted: OriginalSyncRoomEncryptedEvent,
    raw: &Raw<AnySyncTimelineEvent>,
    room: &Room,
    media_dir: &Path,
) -> ChatEvent {
    if let Ok(decrypted) = room.decrypt_event(raw.cast_ref_unchecked(), None).await {
        if let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncMessageLikeEvent::Original(event),
        ))) = decrypted.raw().deserialize()
        {
            return message_event_to_chat(event, room, media_dir).await;
        }
    }

    utd_placeholder(room, &encrypted).await
}

/// Placeholder line for an event we hold but cannot decrypt (missing the Megolm
/// session). Carries the real sender, id and timestamp so it sorts in place and
/// is attributed correctly; only the body stands in for the ciphertext.
async fn utd_placeholder(room: &Room, event: &OriginalSyncRoomEncryptedEvent) -> ChatEvent {
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
async fn report_crypto_status(client: &Client, id: BackendId, events: &EventSender) {
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

/// Reads the persisted map of room-id -> last-shown topic. Returns an empty map
/// when the file is absent (first run) or unreadable.
fn load_known_topics(path: &std::path::Path) -> HashMap<String, String> {
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
fn save_known_topics(path: &std::path::Path, topics: &HashMap<String, String>) {
    match serde_json::to_string(topics) {
        Ok(data) => {
            if let Err(err) = std::fs::write(path, data) {
                log::warn!("failed to persist known topics to {path:?}: {err}");
            }
        }
        Err(err) => log::warn!("failed to serialize known topics: {err}"),
    }
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

    #[test]
    fn media_extension_prefers_filename_then_mime() {
        assert_eq!(media_extension("cat.PNG", None).as_deref(), Some("png"));
        assert_eq!(
            media_extension("report", Some("application/pdf")).as_deref(),
            Some("pdf")
        );
        assert_eq!(media_extension("noext", None), None);
    }

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

    /// Same round-trip as [`login_join_send_roundtrip`], but in an E2E-encrypted
    /// room: a throwaway setup client (a second device of the same user) joins
    /// `TIRC_TEST_ROOM` and turns on encryption, then the backend sends into it.
    /// The backend's own device creates the outbound Megolm session and so can
    /// decrypt its own echo, exercising the encrypt-then-decrypt path end to end.
    /// Run it the same way as the other Matrix tests (see the module doc), with a
    /// `TIRC_TEST_ROOM` you are willing to leave encrypted.
    #[tokio::test]
    #[ignore = "requires the local matrix homeserver from dev/matrix"]
    async fn encrypted_room_send_roundtrip() {
        let homeserver = std::env::var("TIRC_TEST_HOMESERVER").unwrap();
        let user_id = std::env::var("TIRC_TEST_USER").unwrap();
        let password = std::env::var("TIRC_TEST_PASSWORD").unwrap();
        let room = std::env::var("TIRC_TEST_ROOM").unwrap();

        // Turn on encryption out of band so the backend observes an already
        // encrypted room when it syncs.
        let setup = Client::builder()
            .homeserver_url(&homeserver)
            .sqlite_store(unique_store_dir(), None)
            .build()
            .await
            .unwrap();
        setup
            .matrix_auth()
            .login_username(&user_id, &password)
            .initial_device_display_name("tirc-test-setup")
            .await
            .unwrap();
        join(&setup, &room).await.unwrap();
        setup.sync_once(SyncSettings::default()).await.unwrap();
        let setup_room = setup.get_room(&RoomId::parse(&room).unwrap()).unwrap();
        setup_room.enable_encryption().await.unwrap();
        assert!(
            setup_room
                .latest_encryption_state()
                .await
                .unwrap()
                .is_encrypted(),
            "room should be encrypted before the backend sends into it"
        );

        let config = MatrixBackendConfig {
            homeserver,
            user_id,
            password,
            device_id: None,
            autojoin: vec![room.clone()],
            store_dir: Some(unique_store_dir()),
        };

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
                body: "encrypted hello".to_string(),
                kind: MsgKind::Text,
                txn: TxnId(1),
            })
            .unwrap();

        // Match on `id: Some(_)`: the optimistic local echo (emitted before the
        // send) has no event id, so requiring one ensures we waited for the real
        // copy the homeserver round-tripped, i.e. one the backend decrypted.
        assert!(
            wait_for(&mut event_rx, |m| matches!(
                &m.event,
                BackendEvent::Event(ChatEvent::Message { body, id: Some(_), .. })
                    if body.text == "encrypted hello"
            ))
            .await,
            "expected the encrypted message decrypted back through sync"
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
