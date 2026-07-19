//! The Simplified Sliding Sync (MSC4186) driver, used for homeservers that
//! advertise `org.matrix.simplified_msc3575`. It drives the Element X stack -
//! `SyncService` (room list + encryption sync under a supervisor) and, per room,
//! a `Timeline` - instead of the classic `/sync` loop. Rooms stream in as a
//! sorted, windowed list, so the buffer list paints without waiting for the
//! whole account to sync.
//!
//! This module owns only the sliding-specific orchestration; the translation of
//! Matrix content into [`ChatEvent`]s and the SAS verification flow are shared
//! with the classic driver (`convert`, `verify`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::{pin_mut, StreamExt};
use tokio::task::JoinHandle;

use matrix_sdk::ruma::api::client::presence::set_presence;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::ruma::presence::PresenceState;
use matrix_sdk::ruma::{OwnedRoomId, OwnedTransactionId};
use matrix_sdk::{Client, Room};

use matrix_sdk_ui::eyeball_im::VectorDiff;
use matrix_sdk_ui::room_list_service::filters::new_filter_non_left;
use matrix_sdk_ui::room_list_service::RoomListItem;
use matrix_sdk_ui::sync_service::SyncService;
use matrix_sdk_ui::timeline::{
    MembershipChange as TimelineMembershipChange, MsgLikeKind, RoomExt, TimelineItem,
    TimelineItemContent,
};

use tirc_core::backend::{CommandReceiver, EventSender};
use tirc_core::{
    BackendEvent, BackendId, BackendMessage, ChatEvent, Command, EventId, MembershipChange,
    MessageBody, MsgKind, TxnId, UserRef,
};

use crate::convert::{
    already_joined, emit, emit_room_metadata, join, list_public_rooms, load_known_topics,
    msgtype_to_body, room_by_target, room_target, save_known_topics, sender_ref, server_ts,
    spawn_latency_probe,
};
use crate::verify::{apply_verify, register_verification_handler, Verifications};
use crate::MatrixBackendConfig;

/// Number of rooms requested per sliding-sync page. The window grows on demand,
/// but a first page this size keeps the initial buffer list responsive while
/// still covering the rooms a user is realistically looking at.
const PAGE_SIZE: usize = 200;

/// Runs the Simplified Sliding Sync driver to completion: starts the sync
/// service, surfaces rooms from the sorted room list (spawning a timeline
/// consumer per room), and applies outgoing commands until the command channel
/// closes.
pub(crate) async fn run_sliding(
    client: Client,
    id: BackendId,
    config: &MatrixBackendConfig,
    events: EventSender,
    mut commands: CommandReceiver,
    store_path: &Path,
    media_dir: PathBuf,
) -> anyhow::Result<()> {
    // The sync service bundles the room-list sync and the encryption/to-device
    // sync under one supervised task that reconnects on its own.
    let sync_service = SyncService::builder(client.clone()).build().await?;
    sync_service.start().await;

    // SAS verification is shared with the classic driver; the request handler
    // fires on to-device events delivered by the encryption sync.
    let verifications = Verifications::default();
    register_verification_handler(&client, id, events.clone(), verifications.clone());

    let ping_task = spawn_latency_probe(client.clone(), id, events.clone());

    // Autojoin configured rooms not already joined; the room list will surface
    // them once the join is reflected in a sync.
    for room in &config.autojoin {
        if already_joined(&client, room) {
            continue;
        }
        let _ = join(&client, room).await;
    }

    let known_topics_path = store_path.join("known_topics.json");
    let mut announcer = RoomAnnouncer {
        id,
        events: events.clone(),
        media_dir,
        known_topics: load_known_topics(&known_topics_path),
        known_topics_path,
        seen: HashSet::new(),
        timelines: Vec::new(),
    };

    let room_list = sync_service.room_list_service().all_rooms().await?;
    // A filter must be set before the dynamic entries stream yields anything;
    // `non_left` keeps joined/invited/knocked rooms (the ones worth a buffer).
    let (entries, controller) = room_list.entries_with_dynamic_adapters(PAGE_SIZE);
    controller.set_filter(Box::new(new_filter_non_left()));
    pin_mut!(entries);

    let mut announced_synced = false;

    loop {
        tokio::select! {
            diffs = entries.next() => {
                let Some(diffs) = diffs else { break };
                for diff in diffs {
                    announcer.announce_diff(diff).await;
                }
                if !announced_synced {
                    announced_synced = true;
                    let _ = events.send(BackendMessage {
                        backend: id,
                        event: BackendEvent::Synced,
                    });
                }
            }
            command = commands.recv() => {
                let Some(command) = command else { break };
                apply_command(&client, id, &events, &verifications, command).await;
            }
        }
    }

    for timeline in &announcer.timelines {
        timeline.abort();
    }
    ping_task.abort();
    sync_service.stop().await;
    Ok(())
}

/// Tracks which rooms have already been surfaced as buffers and owns the spawned
/// per-room timeline consumers, so a room is announced (and its timeline driven)
/// exactly once even as the room list re-orders it.
struct RoomAnnouncer {
    id: BackendId,
    events: EventSender,
    media_dir: PathBuf,
    known_topics: HashMap<String, String>,
    known_topics_path: PathBuf,
    seen: HashSet<OwnedRoomId>,
    timelines: Vec<JoinHandle<()>>,
}

impl RoomAnnouncer {
    /// Surfaces any not-yet-seen rooms carried by a room-list diff. Only the
    /// room-carrying variants matter: removals leave the buffer in place,
    /// mirroring the classic driver which never tears a buffer down on its own.
    async fn announce_diff(&mut self, diff: VectorDiff<RoomListItem>) {
        match diff {
            VectorDiff::Append { values } | VectorDiff::Reset { values } => {
                for item in values {
                    self.announce_room(&item).await;
                }
            }
            VectorDiff::PushFront { value }
            | VectorDiff::PushBack { value }
            | VectorDiff::Insert { value, .. }
            | VectorDiff::Set { value, .. } => {
                self.announce_room(&value).await;
            }
            VectorDiff::Clear
            | VectorDiff::PopFront
            | VectorDiff::PopBack
            | VectorDiff::Remove { .. }
            | VectorDiff::Truncate { .. } => {}
        }
    }

    /// Emits a room's buffer metadata the first time it is seen and spawns its
    /// timeline consumer. `RoomListItem` derefs to the underlying [`Room`], so
    /// the shared metadata emitter is reused unchanged.
    async fn announce_room(&mut self, item: &RoomListItem) {
        let room: &Room = item;
        if !self.seen.insert(room.room_id().to_owned()) {
            return;
        }
        emit_room_metadata(room, self.id, &self.events, &mut self.known_topics).await;
        save_known_topics(&self.known_topics_path, &self.known_topics);

        self.timelines.push(tokio::spawn(run_room_timeline(
            room.clone(),
            self.id,
            self.events.clone(),
            self.media_dir.clone(),
        )));
    }
}

/// Per-room timeline consumer: builds the room's [`Timeline`] (which owns
/// decryption, edit collapsing, and local echo), then translates its item diffs
/// into [`ChatEvent`]s. The initial item set replaces the classic driver's
/// history backfill; live diffs replace its per-event sync handlers.
async fn run_room_timeline(room: Room, id: BackendId, events: EventSender, media_dir: PathBuf) {
    let timeline = match room.timeline().await {
        Ok(timeline) => timeline,
        Err(err) => {
            log::warn!(
                "could not open timeline for {}: {err}",
                room.room_id()
            );
            return;
        }
    };

    let (initial, stream) = timeline.subscribe().await;
    let mut seen: HashSet<String> = HashSet::new();

    for item in &initial {
        translate_item(item, &room, id, &events, &media_dir, &mut seen, false).await;
    }

    pin_mut!(stream);
    while let Some(diffs) = stream.next().await {
        for diff in diffs {
            match diff {
                VectorDiff::Append { values } => {
                    for item in &values {
                        translate_item(item, &room, id, &events, &media_dir, &mut seen, false)
                            .await;
                    }
                }
                // A reset replaces the whole timeline (e.g. a gappy sync). Clear
                // the seen set and re-emit; `State`'s event-id de-dup absorbs the
                // overlap so nothing doubles.
                VectorDiff::Reset { values } => {
                    seen.clear();
                    for item in &values {
                        translate_item(item, &room, id, &events, &media_dir, &mut seen, false)
                            .await;
                    }
                }
                VectorDiff::PushFront { value }
                | VectorDiff::PushBack { value }
                | VectorDiff::Insert { value, .. } => {
                    translate_item(&value, &room, id, &events, &media_dir, &mut seen, false).await;
                }
                VectorDiff::Set { value, .. } => {
                    translate_item(&value, &room, id, &events, &media_dir, &mut seen, true).await;
                }
                VectorDiff::Clear
                | VectorDiff::PopFront
                | VectorDiff::PopBack
                | VectorDiff::Remove { .. }
                | VectorDiff::Truncate { .. } => {}
            }
        }
    }
}

/// Translates a single timeline item into a [`ChatEvent`]. `is_update` marks a
/// `VectorDiff::Set` (an item changing in place): a message already emitted then
/// re-appearing is an edit, while a first sighting is a new message. Virtual
/// items (date dividers, read markers) and still-unsent local echoes (no event
/// id) are skipped - the optimistic echo emitted on send already covers the
/// latter until it confirms with a real id.
async fn translate_item(
    item: &Arc<TimelineItem>,
    room: &Room,
    id: BackendId,
    events: &EventSender,
    media_dir: &Path,
    seen: &mut HashSet<String>,
    is_update: bool,
) {
    let Some(event) = item.as_event() else { return };
    let Some(event_id) = event.event_id() else {
        return;
    };
    let id_str = event_id.to_string();
    let target = room_target(room);

    match event.content() {
        TimelineItemContent::MsgLike(msglike) => match &msglike.kind {
            MsgLikeKind::Message(message) => {
                let (kind, body) = msgtype_to_body(room, message.msgtype().clone(), media_dir).await;
                if is_update && seen.contains(&id_str) {
                    emit(
                        events,
                        id,
                        ChatEvent::Edit {
                            target,
                            id: EventId(id_str),
                            body,
                        },
                    );
                } else {
                    let echo_of = event
                        .transaction_id()
                        .and_then(|txn| txn.as_str().parse::<u64>().ok())
                        .map(TxnId);
                    emit(
                        events,
                        id,
                        ChatEvent::Message {
                            target,
                            id: Some(EventId(id_str.clone())),
                            sender: sender_ref(room, event.sender()).await,
                            body,
                            kind,
                            echo_of,
                            time: server_ts(event.timestamp()),
                        },
                    );
                    seen.insert(id_str);
                }
            }
            MsgLikeKind::Sticker(sticker) => {
                if is_update && seen.contains(&id_str) {
                    return;
                }
                emit(
                    events,
                    id,
                    ChatEvent::Message {
                        target,
                        id: Some(EventId(id_str.clone())),
                        sender: sender_ref(room, event.sender()).await,
                        body: MessageBody::plain(sticker.content().body.clone()),
                        kind: MsgKind::Text,
                        echo_of: None,
                        time: server_ts(event.timestamp()),
                    },
                );
                seen.insert(id_str);
            }
            MsgLikeKind::UnableToDecrypt(_) => {
                if is_update && seen.contains(&id_str) {
                    return;
                }
                emit(
                    events,
                    id,
                    ChatEvent::Message {
                        target,
                        id: Some(EventId(id_str.clone())),
                        sender: sender_ref(room, event.sender()).await,
                        body: MessageBody::plain(
                            "[unable to decrypt message - encryption keys unavailable]",
                        ),
                        kind: MsgKind::Text,
                        echo_of: None,
                        time: server_ts(event.timestamp()),
                    },
                );
                seen.insert(id_str);
            }
            MsgLikeKind::Redacted => {
                emit(
                    events,
                    id,
                    ChatEvent::Redaction {
                        target,
                        id: EventId(id_str),
                        by: None,
                    },
                );
            }
            // Polls, live location, and other message-likes have no line
            // equivalent yet; skip rather than emit noise.
            MsgLikeKind::Poll(_) | MsgLikeKind::LiveLocation(_) | MsgLikeKind::Other(_) => {}
        },
        TimelineItemContent::MembershipChange(change) => {
            let Some(mapped) = map_membership(change.change(), event.sender()) else {
                return;
            };
            emit(
                events,
                id,
                ChatEvent::Membership {
                    target,
                    who: UserRef {
                        id: change.user_id().to_string(),
                        display: change.display_name(),
                    },
                    change: mapped,
                    time: server_ts(event.timestamp()),
                },
            );
        }
        // Profile changes and room state changes (name/topic/power/...) are not
        // surfaced on the sliding path yet; buffer name and topic still come
        // from the room-list metadata. Tracked as a follow-up.
        _ => {}
    }
}

/// Maps the timeline's rich membership-change enum onto the normalized
/// [`MembershipChange`], attributing invites to the acting sender. Returns
/// `None` for changes we do not surface as a join/leave/invite line.
fn map_membership(
    change: Option<TimelineMembershipChange>,
    sender: &matrix_sdk::ruma::UserId,
) -> Option<MembershipChange> {
    match change? {
        TimelineMembershipChange::Joined | TimelineMembershipChange::InvitationAccepted => {
            Some(MembershipChange::Join { realname: None })
        }
        TimelineMembershipChange::Left
        | TimelineMembershipChange::Kicked
        | TimelineMembershipChange::Banned
        | TimelineMembershipChange::Unbanned
        | TimelineMembershipChange::KickedAndBanned
        | TimelineMembershipChange::InvitationRejected
        | TimelineMembershipChange::InvitationRevoked => {
            Some(MembershipChange::Part { reason: None })
        }
        TimelineMembershipChange::Invited => Some(MembershipChange::Invite {
            by: UserRef::new(sender.as_str()),
        }),
        _ => None,
    }
}

/// Applies an outgoing command on the sliding-sync path. Client-level commands
/// (join/part/topic/list/verify/away) are identical to the classic driver;
/// message send uses the same optimistic-echo-then-send pattern so the echoed
/// event de-duplicates in `State`.
async fn apply_command(
    client: &Client,
    id: BackendId,
    events: &EventSender,
    verifications: &Verifications,
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

            // Optimistic local echo for perceived latency: emit immediately
            // tagged with `txn`, then send with the same Matrix transaction id so
            // the homeserver-echoed event replaces this copy instead of appending
            // a duplicate.
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
        Command::Away { message } => {
            let Some(user_id) = client.user_id() else {
                return;
            };
            let presence = if message.is_some() {
                PresenceState::Unavailable
            } else {
                PresenceState::Online
            };
            let mut request = set_presence::v3::Request::new(user_id.to_owned(), presence);
            request.status_msg = message;
            if let Err(err) = client.send(request).await {
                log::warn!("Away: set_presence failed: {err}");
            }
        }
        // History pagination and reactions move onto the room `Timeline` in a
        // follow-up. Answer FetchHistory so the UI's loading indicator resolves
        // instead of waiting forever.
        Command::FetchHistory { target, .. } => {
            let _ = events.send(BackendMessage {
                backend: id,
                event: BackendEvent::HistoryFetched {
                    target,
                    at_start: false,
                },
            });
        }
        Command::React { .. } => {
            log::warn!("reactions on the sliding-sync path are not implemented yet");
        }
        // IRC-only commands are not handled here.
        _ => {}
    }
}
