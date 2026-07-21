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
use std::sync::{Arc, Mutex};

use futures::{pin_mut, StreamExt};
use tokio::task::JoinHandle;

use matrix_sdk::ruma::api::client::presence::set_presence;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::ruma::events::StateEventContentChange;
use matrix_sdk::ruma::presence::PresenceState;
use matrix_sdk::ruma::{
    EventId as RumaEventId, OwnedEventId, OwnedRoomId, OwnedTransactionId, OwnedUserId, RoomId,
    UserId,
};
use matrix_sdk::{Client, Room};

use matrix_sdk_ui::eyeball_im::VectorDiff;
use matrix_sdk_ui::room_list_service::filters::new_filter_non_left;
use matrix_sdk_ui::room_list_service::RoomListItem;
use matrix_sdk_ui::sync_service::{State as SyncState, SyncService};
use matrix_sdk_ui::timeline::{
    AnyOtherStateEventContentChange, EventTimelineItem,
    MembershipChange as TimelineMembershipChange, MsgLikeKind, ReactionsByKeyBySender, RoomExt,
    Timeline, TimelineEventItemId, TimelineItem, TimelineItemContent,
};

use tirc_core::backend::{CommandReceiver, EventSender};
use tirc_core::{
    BackendEvent, BackendId, BackendMessage, ChatEvent, Command, EventId, MembershipChange,
    MessageBody, MsgKind, TargetId, TxnId, UserRef,
};

use crate::convert::{
    already_joined, describe_room_state_change, emit, emit_room_metadata, join, list_public_rooms,
    load_known_topics, msgtype_to_body, own_or_sender_ref, room_by_target, room_target,
    save_known_topics, sender_ref, server_ts, spawn_latency_probe, RoomStateChange,
};
use crate::verify::{apply_verify, register_verification_handler, Verifications};
use crate::MatrixBackendConfig;

/// Number of rooms requested per sliding-sync page. The window grows on demand,
/// but a first page this size keeps the initial buffer list responsive while
/// still covering the rooms a user is realistically looking at.
const PAGE_SIZE: usize = 200;

/// Timeline events the room-list sync requests per room. The SDK default is 1
/// (just the latest event, for a room-list preview), which leaves buffers with
/// no history; a modest window gives every listed room recent context up front.
const ROOM_LIST_TIMELINE_LIMIT: u32 = 20;

/// Events to backfill when a room's timeline is first opened, so a buffer shows
/// history immediately rather than only what the room-list window carried.
/// Mirrors the classic driver's startup backfill.
const INITIAL_HISTORY: u16 = 30;

/// Per-message reaction snapshot: message event id -> reaction key -> the set of
/// users who reacted with it. Diffed between timeline updates to emit reaction
/// add/remove deltas from the aggregate the timeline exposes.
type ReactionState = HashMap<String, HashMap<String, HashSet<OwnedUserId>>>;

/// Outcome of the sliding-sync driver, so the caller can degrade gracefully.
pub(crate) enum SlidingOutcome {
    /// The driver ran to completion (the command channel closed).
    Completed,
    /// The sliding sync failed (e.g. a homeserver that advertises MSC4186 but
    /// whose implementation the SDK cannot talk to). The command receiver is
    /// handed back so the caller can fall back to the classic driver without
    /// losing queued commands.
    FallBack(CommandReceiver),
}

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
) -> anyhow::Result<SlidingOutcome> {
    // The sync service bundles the room-list sync and the encryption/to-device
    // sync under one supervised task that reconnects on its own. Raise the
    // room-list timeline limit off its default of 1 so listed rooms carry recent
    // history instead of just their latest event.
    let sync_service = SyncService::builder(client.clone())
        .with_room_list_timeline_limit(ROOM_LIST_TIMELINE_LIMIT)
        .build()
        .await?;
    sync_service.start().await;
    let mut service_state = sync_service.state();

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

    // Correlates our own sends to their eventual timeline delivery. The
    // Timeline's own `transaction_id()` on a confirmed item is unreliable (see
    // `PendingEchoes`'s doc), so this is the fallback `translate_item` uses to
    // still replace the optimistic echo in place instead of duplicating it.
    let pending_echoes: PendingEchoes = Arc::new(Mutex::new(HashMap::new()));

    let known_topics_path = store_path.join("known_topics.json");
    let mut announcer = RoomAnnouncer {
        id,
        events: events.clone(),
        media_dir,
        known_topics: load_known_topics(&known_topics_path),
        known_topics_path,
        seen: HashSet::new(),
        timelines: HashMap::new(),
        consumers: Vec::new(),
        pending_echoes: pending_echoes.clone(),
    };

    let room_list = sync_service.room_list_service().all_rooms().await?;
    // A filter must be set before the dynamic entries stream yields anything;
    // `non_left` keeps joined/invited/knocked rooms (the ones worth a buffer).
    let (entries, controller) = room_list.entries_with_dynamic_adapters(PAGE_SIZE);
    controller.set_filter(Box::new(new_filter_non_left()));
    pin_mut!(entries);

    let mut announced_synced = false;

    // Set when the sync service reports a terminal error, so the caller can fall
    // back to the classic driver (e.g. matrix-sdk's simplified sliding sync is
    // incompatible with the homeserver even though it advertised support).
    let mut fall_back = false;

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
                apply_command(
                    &client,
                    id,
                    &events,
                    &verifications,
                    &announcer.timelines,
                    &pending_echoes,
                    command,
                )
                .await;
            }
            Some(state) = service_state.next() => {
                if let SyncState::Error(err) = state {
                    log::warn!("sliding sync failed ({err}); falling back to classic sync");
                    fall_back = true;
                    break;
                }
            }
        }
    }

    for consumer in &announcer.consumers {
        consumer.abort();
    }
    ping_task.abort();
    sync_service.stop().await;

    if fall_back {
        Ok(SlidingOutcome::FallBack(commands))
    } else {
        Ok(SlidingOutcome::Completed)
    }
}

/// Tracks which rooms have already been surfaced as buffers, keeps a handle to
/// each room's [`Timeline`] (so commands can paginate and react), and owns the
/// spawned per-room timeline consumers so a room is driven exactly once even as
/// the room list re-orders it.
struct RoomAnnouncer {
    id: BackendId,
    events: EventSender,
    media_dir: PathBuf,
    known_topics: HashMap<String, String>,
    known_topics_path: PathBuf,
    seen: HashSet<OwnedRoomId>,
    timelines: HashMap<OwnedRoomId, Arc<Timeline>>,
    consumers: Vec<JoinHandle<()>>,
    pending_echoes: PendingEchoes,
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

    /// Emits a room's buffer metadata the first time it is seen, then opens its
    /// timeline and spawns the consumer. `RoomListItem` derefs to the underlying
    /// [`Room`], so the shared metadata emitter is reused unchanged.
    async fn announce_room(&mut self, item: &RoomListItem) {
        let room: &Room = item;
        let room_id = room.room_id().to_owned();
        if !self.seen.insert(room_id.clone()) {
            return;
        }
        emit_room_metadata(room, self.id, &self.events, &mut self.known_topics).await;
        save_known_topics(&self.known_topics_path, &self.known_topics);

        match room.timeline().await {
            Ok(timeline) => {
                let timeline = Arc::new(timeline);
                self.timelines.insert(room_id, timeline.clone());
                self.consumers.push(tokio::spawn(run_room_timeline(
                    timeline,
                    room.clone(),
                    self.id,
                    self.events.clone(),
                    self.media_dir.clone(),
                    self.pending_echoes.clone(),
                )));
            }
            Err(err) => log::warn!("could not open timeline for {}: {err}", room.room_id()),
        }
    }
}

/// Per-room timeline consumer: subscribes to the room's [`Timeline`] (which owns
/// decryption, edit collapsing, and local echo) and translates its item diffs
/// into [`ChatEvent`]s. The initial item set replaces the classic driver's
/// history backfill; live diffs replace its per-event sync handlers.
async fn run_room_timeline(
    timeline: Arc<Timeline>,
    room: Room,
    id: BackendId,
    events: EventSender,
    media_dir: PathBuf,
    pending_echoes: PendingEchoes,
) {
    let (initial, stream) = timeline.subscribe().await;
    let own_user = room.own_user_id().to_owned();
    let mut tracker = TimelineTracker::default();
    let ctx = RoomTimelineCtx {
        id,
        events: &events,
        media_dir: &media_dir,
        pending_echoes: &pending_echoes,
    };

    for item in &initial {
        translate_item(item, &room, &own_user, &ctx, &mut tracker).await;
    }

    // Backfill an initial page so the buffer shows history immediately rather
    // than only the events the room-list window carried. The fetched older
    // events arrive as front-insertions on the subscription stream below.
    if let Err(err) = timeline.paginate_backwards(INITIAL_HISTORY).await {
        log::warn!(
            "initial history backfill for {} failed: {err}",
            room.room_id()
        );
    }

    pin_mut!(stream);
    while let Some(diffs) = stream.next().await {
        for diff in diffs {
            // Every diff that carries item(s) is translated the same way; the
            // tracker de-duplicates by event id, so an item re-delivered by a
            // reset, a metadata-only `Set`, or an overlapping backfill page does
            // not produce a duplicate line. Removals are ignored: buffers are
            // not torn down, mirroring the classic driver.
            match diff {
                VectorDiff::Append { values } | VectorDiff::Reset { values } => {
                    for item in &values {
                        translate_item(item, &room, &own_user, &ctx, &mut tracker).await;
                    }
                }
                VectorDiff::PushFront { value }
                | VectorDiff::PushBack { value }
                | VectorDiff::Insert { value, .. }
                | VectorDiff::Set { value, .. } => {
                    translate_item(&value, &room, &own_user, &ctx, &mut tracker).await;
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

/// Bundles `translate_item`'s per-backend/per-room context so its argument
/// list stays manageable as it grows.
struct RoomTimelineCtx<'a> {
    id: BackendId,
    events: &'a EventSender,
    media_dir: &'a Path,
    pending_echoes: &'a PendingEchoes,
}

/// Correlates our own sends to their eventual confirmed delivery, as a fallback
/// for [`EventTimelineItem::transaction_id`]. That accessor is only populated
/// while the SDK still considers the item a *local* echo; empirically (see the
/// `sliding_send_confirms_with_matching_echo_of` test in `lib.rs`) the
/// item can already have lost its local-echo status - and with it,
/// `transaction_id()` - by the first diff `translate_item` observes for a given
/// event id, especially under the extra latency of a federated room. Without a
/// fallback, `apply_message` never matches the confirmed event back to the
/// pending optimistic echo, leaving it stuck pending forever and duplicating
/// the line. Populated in `apply_command`'s `SendMessage` handling from the
/// send response's real event id (known synchronously, unlike the timeline's
/// own tracking); consumed (and removed) the first time `translate_item` sees
/// that event id.
type PendingEchoes = Arc<Mutex<HashMap<OwnedEventId, TxnId>>>;

/// Per-room de-duplication state for the timeline consumer. The timeline emits a
/// `Set` for an item on any change (read receipts, sender-profile resolution,
/// decryption), not only content edits, and re-delivers items on resets and
/// overlapping backfill pages. Lines are therefore keyed by event id so each is
/// emitted once: state changes, membership, stickers and undecryptable
/// placeholders emit a single line; a message re-emits as an edit only when its
/// body text actually changes; a redaction emits once.
#[derive(Default)]
struct TimelineTracker {
    /// Message event id -> last emitted body text, to detect real edits.
    message_bodies: HashMap<String, String>,
    /// Event ids for which a one-shot line has been emitted.
    emitted: HashSet<String>,
    /// Event ids for which a redaction has been emitted.
    redacted: HashSet<String>,
    /// Per-message reaction snapshot, diffed to emit reaction add/remove deltas.
    reactions: ReactionState,
}

/// Translates a single timeline item into a [`ChatEvent`], de-duplicating by
/// event id via `tracker`. The timeline re-delivers items (resets, metadata-only
/// `Set`s, overlapping backfill), so a message re-emits only as an edit when its
/// body text changes and every other line is emitted once. Virtual items (date
/// dividers, read markers) and still-unsent local echoes (no event id) are
/// skipped - the optimistic echo emitted on send covers the latter until it
/// confirms with a real id.
async fn translate_item(
    item: &Arc<TimelineItem>,
    room: &Room,
    own_user: &UserId,
    ctx: &RoomTimelineCtx<'_>,
    tracker: &mut TimelineTracker,
) {
    let RoomTimelineCtx {
        id,
        events,
        media_dir,
        pending_echoes,
    } = *ctx;
    let Some(event) = item.as_event() else { return };
    let Some(event_id) = event.event_id() else {
        return;
    };
    let id_str = event_id.to_string();
    let target = room_target(room);

    match event.content() {
        TimelineItemContent::MsgLike(msglike) => {
            diff_reactions(
                &id_str,
                &msglike.reactions,
                room,
                own_user,
                &mut tracker.reactions,
                id,
                events,
                &target,
            )
            .await;

            match &msglike.kind {
                MsgLikeKind::Message(message) => {
                    let (kind, body) =
                        msgtype_to_body(room, message.msgtype().clone(), media_dir).await;
                    let previous = tracker.message_bodies.get(&id_str).cloned();
                    match previous {
                        // First sighting: a new message.
                        None => {
                            let echo_of = event
                                .transaction_id()
                                .and_then(|txn| txn.as_str().parse::<u64>().ok())
                                .map(TxnId)
                                .or_else(|| pending_echoes.lock().unwrap().remove(event_id));
                            tracker
                                .message_bodies
                                .insert(id_str.clone(), body.text.clone());
                            emit(
                                events,
                                id,
                                ChatEvent::Message {
                                    target,
                                    id: Some(EventId(id_str)),
                                    sender: sender_ref(room, event.sender()).await,
                                    body,
                                    kind,
                                    echo_of,
                                    time: server_ts(event.timestamp()),
                                },
                            );
                        }
                        // Seen before with different text: a real edit. An
                        // unchanged body means a metadata-only update - skip it.
                        Some(previous) if previous != body.text => {
                            tracker
                                .message_bodies
                                .insert(id_str.clone(), body.text.clone());
                            emit(
                                events,
                                id,
                                ChatEvent::Edit {
                                    target,
                                    id: EventId(id_str),
                                    body,
                                },
                            );
                        }
                        Some(_) => {}
                    }
                }
                MsgLikeKind::Sticker(sticker) => {
                    if tracker.emitted.insert(id_str.clone()) {
                        emit(
                            events,
                            id,
                            ChatEvent::Message {
                                target,
                                id: Some(EventId(id_str)),
                                sender: sender_ref(room, event.sender()).await,
                                body: MessageBody::plain(sticker.content().body.clone()),
                                kind: MsgKind::Text,
                                echo_of: None,
                                time: server_ts(event.timestamp()),
                            },
                        );
                    }
                }
                MsgLikeKind::UnableToDecrypt(_) => {
                    if tracker.emitted.insert(id_str.clone()) {
                        emit(
                            events,
                            id,
                            ChatEvent::Message {
                                target,
                                id: Some(EventId(id_str)),
                                sender: sender_ref(room, event.sender()).await,
                                body: MessageBody::plain(
                                    "[unable to decrypt message - encryption keys unavailable]",
                                ),
                                kind: MsgKind::Text,
                                echo_of: None,
                                time: server_ts(event.timestamp()),
                            },
                        );
                    }
                }
                MsgLikeKind::Redacted => {
                    if tracker.redacted.insert(id_str.clone()) {
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
                }
                // Polls, live location, and other message-likes have no line
                // equivalent yet; skip rather than emit noise.
                MsgLikeKind::Poll(_) | MsgLikeKind::LiveLocation(_) | MsgLikeKind::Other(_) => {}
            }
        }
        TimelineItemContent::MembershipChange(change) => {
            if !tracker.emitted.insert(id_str) {
                return;
            }
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
        TimelineItemContent::OtherState(other) if tracker.emitted.insert(id_str) => {
            translate_other_state(other.content(), room, event, id, events).await;
        }
        // Profile-only changes are intentionally not surfaced, matching the
        // classic driver (which suppresses member profile changes). Failed-to-
        // parse events and call notifications have no line equivalent.
        _ => {}
    }
}

/// Renders a room state change (name/topic/power/join-rule/...) as a line,
/// reusing the shared wording. A name or topic change also updates the buffer's
/// tab label / topic, mirroring the classic driver's dedicated handlers.
async fn translate_other_state(
    other: &AnyOtherStateEventContentChange,
    room: &Room,
    event: &EventTimelineItem,
    id: BackendId,
    events: &EventSender,
) {
    let target = room_target(room);
    let time = server_ts(event.timestamp());
    let actor = sender_ref(room, event.sender()).await;
    let actor_name = actor.display.clone().unwrap_or_else(|| actor.id.clone());

    match other {
        AnyOtherStateEventContentChange::RoomName(StateEventContentChange::Original {
            content,
            ..
        }) => {
            // Keep the tab label in sync; an empty name falls back to the
            // member-derived display name, matching the classic name handler.
            let name = if content.name.trim().is_empty() {
                room.display_name()
                    .await
                    .map(|name| name.to_string())
                    .unwrap_or_else(|_| target.0.clone())
            } else {
                content.name.clone()
            };
            emit(
                events,
                id,
                ChatEvent::BufferName {
                    target: target.clone(),
                    name,
                },
            );
        }
        AnyOtherStateEventContentChange::RoomTopic(StateEventContentChange::Original {
            content,
            ..
        }) => {
            // The topic line renders the change; no separate descriptive line.
            emit(
                events,
                id,
                ChatEvent::Topic {
                    target,
                    who: Some(actor),
                    topic: content.topic.clone(),
                    time,
                },
            );
            return;
        }
        _ => {}
    }

    if let Some((code, change)) = map_other_state(other) {
        emit(
            events,
            id,
            ChatEvent::ServerInfo {
                target: Some(target),
                from: Some(actor_name.clone()),
                code: Some(code.to_string()),
                text: describe_room_state_change(&actor_name, &change),
                raw: None,
                time,
            },
        );
    }
}

/// Extracts the event type and a [`RoomStateChange`] from a timeline state
/// change, or `None` for changes we do not describe (redacted ones, and types
/// without a line). Mirrors the classic driver's `room_state_change`, but over
/// the timeline's content-change enum.
fn map_other_state(
    other: &AnyOtherStateEventContentChange,
) -> Option<(&'static str, RoomStateChange<'_>)> {
    use AnyOtherStateEventContentChange as Other;
    match other {
        Other::RoomCreate(StateEventContentChange::Original { .. }) => {
            Some(("m.room.create", RoomStateChange::Created))
        }
        Other::RoomName(StateEventContentChange::Original { content, .. }) => {
            Some(("m.room.name", RoomStateChange::Renamed(&content.name)))
        }
        Other::RoomPowerLevels(StateEventContentChange::Original { .. }) => {
            Some(("m.room.power_levels", RoomStateChange::PowerLevels))
        }
        Other::RoomJoinRules(StateEventContentChange::Original { content, .. }) => Some((
            "m.room.join_rules",
            RoomStateChange::JoinRule(content.join_rule.as_str()),
        )),
        Other::RoomHistoryVisibility(StateEventContentChange::Original { content, .. }) => Some((
            "m.room.history_visibility",
            RoomStateChange::HistoryVisibility(content.history_visibility.as_str()),
        )),
        Other::RoomGuestAccess(StateEventContentChange::Original { content, .. }) => Some((
            "m.room.guest_access",
            RoomStateChange::GuestAccess(content.guest_access.as_str()),
        )),
        Other::RoomAvatar(StateEventContentChange::Original { content, .. }) => Some((
            "m.room.avatar",
            RoomStateChange::Avatar {
                removed: content.url.is_none(),
            },
        )),
        _ => None,
    }
}

/// Emits reaction add/remove deltas for a message by diffing the timeline's
/// aggregate reaction set against the last snapshot we saw for it. The timeline
/// exposes reactions as a materialized `key -> senders` map, so add/remove
/// events are recovered by comparing successive snapshots.
#[allow(clippy::too_many_arguments)]
async fn diff_reactions(
    id_str: &str,
    current: &ReactionsByKeyBySender,
    room: &Room,
    own_user: &UserId,
    state: &mut ReactionState,
    id: BackendId,
    events: &EventSender,
    target: &TargetId,
) {
    let mut next: HashMap<String, HashSet<OwnedUserId>> = HashMap::new();
    for (key, senders) in current.iter() {
        next.insert(key.clone(), senders.keys().cloned().collect());
    }

    let previous = state.get(id_str).cloned().unwrap_or_default();

    for (key, senders) in &next {
        for user in senders {
            let existed = previous.get(key).is_some_and(|set| set.contains(user));
            if !existed {
                emit_reaction(id_str, key, user, room, own_user, id, events, target, true).await;
            }
        }
    }
    for (key, senders) in &previous {
        for user in senders {
            let still_present = next.get(key).is_some_and(|set| set.contains(user));
            if !still_present {
                emit_reaction(id_str, key, user, room, own_user, id, events, target, false).await;
            }
        }
    }

    if next.is_empty() {
        state.remove(id_str);
    } else {
        state.insert(id_str.to_string(), next);
    }
}

/// Emits a single reaction add/remove line, attributing it to the reacting user
/// (or the local user's registered nickname for our own reactions, matching the
/// classic driver so `State` can mark it `mine`).
#[allow(clippy::too_many_arguments)]
async fn emit_reaction(
    id_str: &str,
    key: &str,
    user: &UserId,
    room: &Room,
    own_user: &UserId,
    id: BackendId,
    events: &EventSender,
    target: &TargetId,
    add: bool,
) {
    let mine = user == own_user;
    emit(
        events,
        id,
        ChatEvent::Reaction {
            target: target.clone(),
            id: EventId(id_str.to_string()),
            sender: own_or_sender_ref(room, user, mine).await,
            key: key.to_string(),
            add,
        },
    );
}

/// Maps the timeline's rich membership-change enum onto the normalized
/// [`MembershipChange`], attributing invites to the acting sender. Returns
/// `None` for changes we do not surface as a join/leave/invite line.
fn map_membership(
    change: Option<TimelineMembershipChange>,
    sender: &UserId,
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
/// event de-duplicates in `State`. History and reactions act on the room's
/// [`Timeline`], looked up by target.
async fn apply_command(
    client: &Client,
    id: BackendId,
    events: &EventSender,
    verifications: &Verifications,
    timelines: &HashMap<OwnedRoomId, Arc<Timeline>>,
    pending_echoes: &PendingEchoes,
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
            // Record the real event id as soon as the send response gives it to
            // us, so `translate_item` can still recognize this as our own echo
            // even if the Timeline's own `transaction_id()` has already been
            // dropped by the time it delivers the confirmed event (see
            // `PendingEchoes`).
            if let Ok(result) = room.send(content).with_transaction_id(transaction_id).await {
                pending_echoes
                    .lock()
                    .unwrap()
                    .insert(result.response.event_id, txn);
            }
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
        Command::FetchHistory { target, limit, .. } => {
            let at_start = match timeline_for(timelines, &target) {
                Some(timeline) => timeline.paginate_backwards(limit).await.unwrap_or(false),
                None => false,
            };
            let _ = events.send(BackendMessage {
                backend: id,
                event: BackendEvent::HistoryFetched { target, at_start },
            });
        }
        Command::React {
            target,
            id: event_id,
            key,
            add,
        } => {
            let Some(timeline) = timeline_for(timelines, &target) else {
                return;
            };
            let parsed = match RumaEventId::parse(&event_id.0) {
                Ok(parsed) => parsed,
                Err(err) => {
                    log::warn!("React: invalid event id {}: {err}", event_id.0);
                    return;
                }
            };
            // `toggle_reaction` flips the current state, so only act when the
            // desired state differs from what we already have - otherwise a
            // redundant add/remove would invert the reaction.
            let own = client.user_id();
            if own_reacted(timeline, &parsed, &key, own).await == add {
                return;
            }
            let item = TimelineEventItemId::EventId(parsed);
            if let Err(err) = timeline.toggle_reaction(&item, &key).await {
                log::warn!("React toggle failed: {err}");
            }
        }
        // IRC-only commands are not handled here.
        _ => {}
    }
}

/// Looks up the room `Timeline` for a target id, if the room has been surfaced.
fn timeline_for<'a>(
    timelines: &'a HashMap<OwnedRoomId, Arc<Timeline>>,
    target: &TargetId,
) -> Option<&'a Arc<Timeline>> {
    let room_id = RoomId::parse(target.as_str()).ok()?;
    timelines.get(&room_id)
}

/// Whether the local user has already reacted to `event_id` with `key`, read
/// from the timeline's current item, so a React command can skip a no-op toggle.
async fn own_reacted(
    timeline: &Timeline,
    event_id: &RumaEventId,
    key: &str,
    own: Option<&UserId>,
) -> bool {
    let Some(own) = own else { return false };
    let Some(item) = timeline.item_by_event_id(event_id).await else {
        return false;
    };
    let Some(msglike) = item.content().as_msglike() else {
        return false;
    };
    msglike
        .reactions
        .get(key)
        .is_some_and(|senders| senders.keys().any(|user| user.as_str() == own.as_str()))
}
