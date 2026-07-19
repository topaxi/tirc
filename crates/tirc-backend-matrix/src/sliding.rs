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

use std::collections::HashSet;
use std::path::Path;

use futures::{pin_mut, StreamExt};

use matrix_sdk::ruma::api::client::presence::set_presence;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::ruma::presence::PresenceState;
use matrix_sdk::ruma::{OwnedRoomId, OwnedTransactionId};
use matrix_sdk::{Client, Room};

use matrix_sdk_ui::eyeball_im::VectorDiff;
use matrix_sdk_ui::room_list_service::filters::new_filter_non_left;
use matrix_sdk_ui::room_list_service::RoomListItem;
use matrix_sdk_ui::sync_service::SyncService;

use tirc_core::backend::{CommandReceiver, EventSender};
use tirc_core::{
    BackendEvent, BackendId, BackendMessage, ChatEvent, Command, MessageBody, MsgKind, UserRef,
};

use crate::convert::{
    already_joined, emit, emit_room_metadata, join, list_public_rooms, load_known_topics,
    room_by_target, save_known_topics, spawn_latency_probe,
};
use crate::verify::{apply_verify, register_verification_handler, Verifications};
use crate::MatrixBackendConfig;

/// Number of rooms requested per sliding-sync page. The window grows on demand,
/// but a first page this size keeps the initial buffer list responsive while
/// still covering the rooms a user is realistically looking at.
const PAGE_SIZE: usize = 200;

/// Runs the Simplified Sliding Sync driver to completion: starts the sync
/// service, surfaces rooms from the sorted room list as they arrive, and applies
/// outgoing commands until the command channel closes.
pub(crate) async fn run_sliding(
    client: Client,
    id: BackendId,
    config: &MatrixBackendConfig,
    events: EventSender,
    mut commands: CommandReceiver,
    store_path: &Path,
    _media_dir: std::path::PathBuf,
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
    let mut known_topics = load_known_topics(&known_topics_path);

    let room_list = sync_service.room_list_service().all_rooms().await?;
    // A filter must be set before the dynamic entries stream yields anything;
    // `non_left` keeps joined/invited/knocked rooms (the ones worth a buffer).
    let (entries, controller) = room_list.entries_with_dynamic_adapters(PAGE_SIZE);
    controller.set_filter(Box::new(new_filter_non_left()));
    pin_mut!(entries);

    let mut seen: HashSet<OwnedRoomId> = HashSet::new();
    let mut announced_synced = false;

    loop {
        tokio::select! {
            diffs = entries.next() => {
                let Some(diffs) = diffs else { break };
                for diff in diffs {
                    announce_diff(diff, id, &events, &mut seen, &mut known_topics).await;
                }
                save_known_topics(&known_topics_path, &known_topics);
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

    ping_task.abort();
    sync_service.stop().await;
    Ok(())
}

/// Surfaces any not-yet-seen rooms carried by a room-list diff as buffers. Only
/// the room-carrying variants matter here: removals leave the buffer in place
/// (mirroring the classic driver, which never tears a buffer down on its own).
async fn announce_diff(
    diff: VectorDiff<RoomListItem>,
    id: BackendId,
    events: &EventSender,
    seen: &mut HashSet<OwnedRoomId>,
    known_topics: &mut std::collections::HashMap<String, String>,
) {
    match diff {
        VectorDiff::Append { values } | VectorDiff::Reset { values } => {
            for item in values {
                announce_room(&item, id, events, seen, known_topics).await;
            }
        }
        VectorDiff::PushFront { value }
        | VectorDiff::PushBack { value }
        | VectorDiff::Insert { value, .. }
        | VectorDiff::Set { value, .. } => {
            announce_room(&value, id, events, seen, known_topics).await;
        }
        VectorDiff::Clear
        | VectorDiff::PopFront
        | VectorDiff::PopBack
        | VectorDiff::Remove { .. }
        | VectorDiff::Truncate { .. } => {}
    }
}

/// Emits a room's buffer metadata the first time it is seen. `RoomListItem`
/// derefs to the underlying [`Room`], so the shared metadata emitter is reused
/// unchanged.
async fn announce_room(
    item: &RoomListItem,
    id: BackendId,
    events: &EventSender,
    seen: &mut HashSet<OwnedRoomId>,
    known_topics: &mut std::collections::HashMap<String, String>,
) {
    let room: &Room = item;
    if !seen.insert(room.room_id().to_owned()) {
        return;
    }
    emit_room_metadata(room, id, events, known_topics).await;
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
