//! The classic `/sync` driver: today's sync-loop implementation, used for
//! homeservers without Simplified Sliding Sync support. Registers per-event-type
//! handlers on the SDK sync loop, backfills history via `room.messages`, and
//! applies outgoing commands directly to the client.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;

use matrix_sdk::config::SyncSettings;
use matrix_sdk::deserialized_responses::TimelineEvent;
use matrix_sdk::room::MessagesOptions;
use matrix_sdk::ruma::api::client::presence::set_presence;
use matrix_sdk::ruma::events::reaction::ReactionEventContent;
use matrix_sdk::ruma::events::relation::Annotation;
use matrix_sdk::ruma::events::room::encrypted::{
    OriginalSyncRoomEncryptedEvent, SyncRoomEncryptedEvent,
};
use matrix_sdk::ruma::events::room::member::MembershipState;
use matrix_sdk::ruma::events::room::member::SyncRoomMemberEvent;
use matrix_sdk::ruma::events::room::message::{RoomMessageEventContent, SyncRoomMessageEvent};
use matrix_sdk::ruma::events::room::name::SyncRoomNameEvent;
use matrix_sdk::ruma::events::room::power_levels::SyncRoomPowerLevelsEvent;
use matrix_sdk::ruma::events::room::redaction::SyncRoomRedactionEvent;
use matrix_sdk::ruma::events::room::topic::SyncRoomTopicEvent;
use matrix_sdk::ruma::events::{
    AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent,
};
use matrix_sdk::ruma::presence::PresenceState;
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::ruma::OwnedTransactionId;
use matrix_sdk::{Client, Room};

use tirc_core::backend::{CommandReceiver, EventSender};
use tirc_core::{
    BackendEvent, BackendId, BackendMessage, BufferKind, ChatEvent, Command, EventId,
    MembershipChange, MessageBody, MsgKind, TargetId, UserRef,
};

use crate::convert::{
    already_joined, emit, is_handled_timeline_event, is_server_notice_room, join,
    list_public_rooms, load_known_topics, message_event_to_chat, own_or_sender_ref,
    role_from_power, room_by_target, room_can_post, room_event_line, room_target,
    save_known_topics, sender_ref, server_ts, status_line, trim_display_name, utd_placeholder,
};
use crate::verify::{apply_verify, register_verification_handler, Verifications};
use crate::MatrixBackendConfig;

/// Runs the classic `/sync` driver to completion: initial sync, room population,
/// handler registration, the background sync/ping tasks, and the command loop.
pub(crate) async fn run_classic(
    client: Client,
    id: BackendId,
    config: &MatrixBackendConfig,
    events: EventSender,
    mut commands: CommandReceiver,
    store_path: &Path,
    media_dir: PathBuf,
) -> anyhow::Result<()> {
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
    let history_cursors = HistoryCursors::default();
    for room in joined {
        populate_room(
            &room,
            id,
            &events,
            &mut known_topics,
            &media_dir,
            &history_cursors,
        )
        .await;
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
    // token (set by sync_once) so it delivers only new events. Every sync
    // long-poll advertises a presence state to the homeserver (the default
    // is Online, which would revert an away presence within one poll), so
    // the desired state is tracked in a watch channel and the in-flight
    // sync is restarted whenever `Command::Away` changes it. Cancelling a
    // sync mid-poll is safe: the token only advances on processed
    // responses, so the restart resumes from the same point.
    let (presence_tx, mut presence_rx) = tokio::sync::watch::channel(PresenceState::Online);
    let sync_client = client.clone();
    let sync = tokio::spawn(async move {
        loop {
            let desired = presence_rx.borrow_and_update().clone();
            tokio::select! {
                _ = sync_client.sync(SyncSettings::default().set_presence(desired)) => break,
                _ = presence_rx.changed() => continue,
            }
        }
    });

    // Periodic round-trip probe: call whoami() every 30s and emit the RTT
    // as a Latency event. After 3 consecutive failures, emit Disconnected so
    // the buffer bar shows an offline indicator (the SDK retries sync internally
    // so we won't get an explicit Disconnected otherwise). The transitions are
    // edge-triggered via `degraded`: Disconnected is emitted once when the probe
    // starts failing, and a visible "Connection restored" line once it recovers
    // (the Latency event alone silently flips the bar back to Connected, so
    // without this the reconnection - which the SDK performs transparently -
    // would never appear in the status buffer, unlike IRC's reconnect logging).
    let ping_client = client.clone();
    let ping_events = events.clone();
    let ping_task = tokio::spawn(async move {
        let mut interval = std::time::Duration::from_secs(30);
        let mut failures: u32 = 0;
        let mut degraded = false;
        loop {
            tokio::time::sleep(interval).await;
            interval = std::time::Duration::from_secs(30);
            let start = std::time::Instant::now();
            if ping_client.whoami().await.is_ok() {
                failures = 0;
                if degraded {
                    degraded = false;
                    emit(
                        &ping_events,
                        id,
                        status_line("Connection restored".to_string()),
                    );
                }
                let ms = start.elapsed().as_millis() as u64;
                let _ = ping_events.send(BackendMessage {
                    backend: id,
                    event: BackendEvent::Latency { ms },
                });
            } else {
                failures += 1;
                if failures >= 3 && !degraded {
                    degraded = true;
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
    for room in &config.autojoin {
        if already_joined(&client, room) {
            continue;
        }
        let _ = join(&client, room).await;
    }

    while let Some(command) = commands.recv().await {
        apply_command(
            &client,
            id,
            &events,
            &verifications,
            &reactions,
            &history_cursors,
            &media_dir,
            &presence_tx,
            command,
        )
        .await;
    }

    sync.abort();
    ping_task.abort();
    Ok(())
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
            emit(&events, id, room_event_line(&room, &event).await);
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

    // Keep the buffer's tab label in sync when the room is renamed. The descriptive
    // "changed the room name" line is still emitted by the catch-all; this only
    // updates the tab name (silently), mirroring how the topic handler both renders
    // a line and persists the new topic. An empty name clears the room name, so the
    // canonical display name (member-derived) is recomputed instead.
    let name_events = events.clone();
    client.add_event_handler(move |event: SyncRoomNameEvent, room: Room| {
        let events = name_events.clone();
        async move {
            if let SyncRoomNameEvent::Original(event) = event {
                let name = event.content.name;
                let name = if name.is_empty() {
                    room.display_name()
                        .await
                        .map(|name| name.to_string())
                        .unwrap_or_else(|_| room_target(&room).0)
                } else {
                    name
                };
                emit(
                    &events,
                    id,
                    ChatEvent::BufferName {
                        target: room_target(&room),
                        name,
                    },
                );
            }
        }
    });

    // Recompute the local user's post permission when a room's power levels
    // change, so the read-only input hint appears/clears live rather than only at
    // startup (populate_room seeds the initial value). The descriptive
    // "changed the power levels" line is still emitted by the catch-all.
    let power_events = events.clone();
    client.add_event_handler(move |event: SyncRoomPowerLevelsEvent, room: Room| {
        let events = power_events.clone();
        async move {
            if let SyncRoomPowerLevelsEvent::Original(_) = event {
                emit(
                    &events,
                    id,
                    ChatEvent::BufferPostPolicy {
                        target: room_target(&room),
                        can_post: room_can_post(&room).await,
                    },
                );
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

    register_verification_handler(client, id, events, verifications);
}

/// Per-room pagination cursor for older-history fetches: `Some(token)` = the
/// next page token from the last `room.messages` call, `None` = the start of
/// the timeline was reached. Absent = never paginated (the next fetch starts
/// from the newest messages; event-id dedup absorbs the overlap with the
/// startup backfill).
type HistoryCursors = Arc<Mutex<HashMap<String, Option<String>>>>;

/// Applies an outgoing command to the Matrix client.
#[allow(clippy::too_many_arguments)]
async fn apply_command(
    client: &Client,
    id: BackendId,
    events: &EventSender,
    verifications: &Verifications,
    reactions: &ReactionIndex,
    history_cursors: &HistoryCursors,
    media_dir: &Path,
    presence_tx: &tokio::sync::watch::Sender<PresenceState>,
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
        Command::FetchHistory { target, limit, .. } => {
            fetch_history(
                client,
                id,
                events,
                history_cursors,
                media_dir,
                target,
                limit,
            )
            .await;
        }
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
        Command::Away { message } => {
            let Some(user_id) = client.user_id() else {
                return;
            };
            let presence = if message.is_some() {
                PresenceState::Unavailable
            } else {
                PresenceState::Online
            };
            // Keep the sync loop advertising the same state, or the next
            // long-poll would revert the presence set below.
            let _ = presence_tx.send(presence.clone());
            let mut request = set_presence::v3::Request::new(user_id.to_owned(), presence);
            request.status_msg = message;
            if let Err(err) = client.send(request).await {
                log::warn!("Away: set_presence failed: {err}");
            }
        }
        // IRC-only commands are not handled here.
        _ => {}
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
    history_cursors: &HistoryCursors,
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

    backfill_room(room, id, events, media_dir, history_cursors).await;
}

/// Backfills the most recent messages of a room (oldest-first) so freshly-opened
/// buffers show history instead of being empty until new activity.
async fn backfill_room(
    room: &Room,
    id: BackendId,
    events: &EventSender,
    media_dir: &Path,
    history_cursors: &HistoryCursors,
) {
    let mut options = MessagesOptions::backward();
    options.limit = 30u32.into();

    let Ok(messages) = room.messages(options).await else {
        return;
    };

    // Seed the pagination cursor so scroll-triggered fetches continue where
    // this startup page ended instead of re-fetching the newest messages.
    history_cursors
        .lock()
        .await
        .insert(room.room_id().to_string(), messages.end.clone());

    emit_history_chunk(messages.chunk, room, id, events, media_dir).await;
}

/// Emits a page of `room.messages` results as chat events. `chunk` is
/// newest-first; translated messages are emitted oldest-first so `State`'s
/// sorted insert sees them in natural order.
async fn emit_history_chunk(
    chunk: Vec<TimelineEvent>,
    room: &Room,
    id: BackendId,
    events: &EventSender,
    media_dir: &Path,
) {
    let mut chats = Vec::new();
    for timeline_event in chunk {
        if let Some(chat) = backfill_event_to_chat(timeline_event, room, media_dir).await {
            chats.push(chat);
        }
    }

    for chat in chats.into_iter().rev() {
        emit(events, id, chat);
    }
}

/// Handles [`Command::FetchHistory`]: pages the room's timeline backward from
/// the stored cursor. Every path answers with [`BackendEvent::HistoryFetched`].
async fn fetch_history(
    client: &Client,
    id: BackendId,
    events: &EventSender,
    history_cursors: &HistoryCursors,
    media_dir: &Path,
    target: TargetId,
    limit: u16,
) {
    let done = |at_start: bool| BackendMessage {
        backend: id,
        event: BackendEvent::HistoryFetched {
            target: target.clone(),
            at_start,
        },
    };

    let Some(room) = room_by_target(client, &target) else {
        let _ = events.send(done(false));
        return;
    };

    let from = match history_cursors
        .lock()
        .await
        .get(room.room_id().as_str())
        .cloned()
    {
        // Start of the timeline was already reached.
        Some(None) => {
            let _ = events.send(done(true));
            return;
        }
        Some(Some(token)) => Some(token),
        // Never paginated (e.g. the room joined after startup): fetch the
        // newest page; event-id dedup absorbs any overlap.
        None => None,
    };

    let mut options = MessagesOptions::backward();
    options.from = from;
    options.limit = u32::from(limit).into();

    match room.messages(options).await {
        Ok(messages) => {
            // Only a missing end token means the start of the timeline: the spec
            // omits `end` when nothing further is available. An empty chunk with
            // a token is a legitimate mid-history page (e.g. events invisible to
            // us), so it must not permanently mark the room exhausted.
            let at_start = messages.end.is_none();
            history_cursors
                .lock()
                .await
                .insert(room.room_id().to_string(), messages.end.clone());
            emit_history_chunk(messages.chunk, &room, id, events, media_dir).await;
            let _ = events.send(done(at_start));
        }
        Err(err) => {
            // Retryable: keep the buffer non-exhausted so the user can try again.
            log::warn!("FetchHistory for {} failed: {err}", target.as_str());
            let _ = events.send(done(false));
        }
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
        // skipped; anything else (including recognized room state changes) surfaces
        // as a line, timestamped from the event so it sorts in place in history.
        Ok(other) if is_handled_timeline_event(&other) => None,
        Ok(other) => Some(room_event_line(room, &other).await),
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
