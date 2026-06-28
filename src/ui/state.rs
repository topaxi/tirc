use chrono::{DateTime, Local};
use indexmap::IndexMap;

use crate::backends::BackendInfo;
use crate::core::{
    BackendId, BufferId, ChatEvent, EventId, MemberRole, MembershipChange, MessageBody, TargetId,
    TxnId, UserRef,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Normal,
    Command,
    Insert,
}

/// A member of a buffer's roster. Ordered by `(role, name)` so the user list is
/// a pure read of an already-sorted vector.
#[derive(Clone, Debug)]
pub struct Member {
    pub user: UserRef,
    pub role: MemberRole,
}

/// A single stored line. Holds the normalized [`ChatEvent`] only - never an
/// `mlua` table - so domain state stays free of Lua and could later cross a
/// process boundary. The renderer builds the Lua view per-frame.
#[derive(Clone, Debug)]
pub struct StoredMessage {
    pub time: DateTime<Local>,
    pub event: ChatEvent,
    /// Optimistic local echo not yet confirmed by the server.
    pub pending: bool,
    pub redacted: bool,
    /// Reaction key -> count. Populated by Matrix; unused by IRC.
    pub reactions: IndexMap<String, u32>,
}

impl StoredMessage {
    fn new(event: ChatEvent, pending: bool) -> Self {
        StoredMessage {
            time: Local::now(),
            event,
            pending,
            redacted: false,
            reactions: IndexMap::new(),
        }
    }

    /// A non-pending stored message stamped with a server time when known,
    /// falling back to `Local::now()` otherwise. Used for events that carry their
    /// own timestamp (messages, membership, topic) so they sort chronologically.
    fn new_at(event: ChatEvent, time: Option<DateTime<chrono::Utc>>) -> Self {
        let mut message = StoredMessage::new(event, false);
        if let Some(time) = time {
            message.time = time.with_timezone(&Local);
        }
        message
    }

    /// A non-pending stored message, used to build a Lua view of an event that is
    /// not (yet) in a buffer, e.g. when firing the `event` callback.
    pub fn from_event(event: ChatEvent) -> Self {
        StoredMessage::new(event, false)
    }

    fn txn(&self) -> Option<TxnId> {
        match &self.event {
            ChatEvent::Message { echo_of, .. } => *echo_of,
            _ => None,
        }
    }

    fn has_event_id(&self, id: &EventId) -> bool {
        matches!(&self.event, ChatEvent::Message { id: Some(existing), .. } if existing == id)
    }
}

#[derive(Debug, Default)]
pub struct ChatBuffer {
    pub messages: Vec<StoredMessage>,
    pub members: Vec<Member>,
    pub topic: Option<String>,
    /// Friendly name shown instead of the raw target (e.g. a Matrix room name).
    pub display_name: Option<String>,
    pub scroll_position: usize,
    /// True when messages have arrived that the user has not yet seen.
    pub has_unread: bool,
    /// True when the user's nickname was mentioned in an unseen message.
    pub has_mention: bool,
    /// Timestamp of the newest message when the user last left or was actively
    /// viewing this buffer. Messages newer than this marker are shown below a
    /// visual "new messages" separator in the message list.
    pub read_marker: Option<DateTime<Local>>,
}

impl ChatBuffer {
    /// The name to show for this buffer: its display name, else the raw target.
    pub fn label<'a>(&'a self, target: &'a TargetId) -> &'a str {
        self.display_name
            .as_deref()
            .unwrap_or_else(|| target.as_str())
    }
}

impl ChatBuffer {
    /// Scroll toward older messages. Clamped so the oldest message stays
    /// visible - advancing past it would produce a blank screen.
    pub fn scroll_up(&mut self, lines: usize) {
        let max = self.messages.len().saturating_sub(1);
        self.scroll_position = self.scroll_position.saturating_add(lines).min(max);
    }

    /// Scroll toward newer messages. Clamps at 0 (the tail).
    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_position = self.scroll_position.saturating_sub(lines);
    }

    /// Scroll so the oldest messages fill a full viewport. Requires the
    /// current viewport height so the oldest line lands at the top, not the bottom.
    pub fn scroll_to_top(&mut self, viewport_height: usize) {
        self.scroll_position = self.messages.len().saturating_sub(viewport_height);
    }

    /// Return to the newest messages.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_position = 0;
    }

    /// Clears the unread and mention indicators. Does not touch `read_marker`
    /// so the separator remains visible while the user reads the new messages.
    pub fn mark_read(&mut self) {
        self.has_unread = false;
        self.has_mention = false;
    }

    /// Advances `read_marker` to the newest message, establishing the boundary
    /// for the next unread separator. Call when the user leaves a buffer or
    /// when messages arrive while the buffer is focused (user is watching).
    pub fn advance_read_marker(&mut self) {
        if let Some(newest) = self.messages.last() {
            self.read_marker = Some(newest.time);
        }
    }

    /// Inserts a message keeping `messages` sorted by `time`. The scan runs from
    /// the end so the common append case (a live message newer than everything)
    /// is O(1), and equal-time messages keep arrival order (the new one lands
    /// *after* existing ones of the same timestamp). Every path that adds a line
    /// must go through here so the sorted invariant the renderer relies on is
    /// never broken by a direct `push`.
    fn insert_message(&mut self, message: StoredMessage) {
        let pos = self
            .messages
            .iter()
            .rposition(|m| m.time <= message.time)
            .map(|i| i + 1)
            .unwrap_or(0);
        self.messages.insert(pos, message);
    }

    /// Inserts or updates a member, returning `true` only when the member was not
    /// already in the roster. Callers use this to suppress a redundant "has
    /// joined" line for a member that is already present (e.g. a re-delivered or
    /// self membership event).
    fn upsert_member(&mut self, user: UserRef, role: MemberRole) -> bool {
        let is_new = match self.members.iter_mut().find(|m| m.user.id == user.id) {
            Some(member) => {
                member.user = user;
                member.role = role;
                false
            }
            None => {
                self.members.push(Member { user, role });
                true
            }
        };
        self.sort_members();
        is_new
    }

    fn set_member_role(&mut self, id: &str, role: MemberRole) {
        if let Some(member) = self.members.iter_mut().find(|m| m.user.id == id) {
            member.role = role;
            self.sort_members();
        }
    }

    fn remove_member(&mut self, id: &str) -> bool {
        let before = self.members.len();
        self.members.retain(|m| m.user.id != id);
        self.members.len() != before
    }

    fn rename_member(&mut self, id: &str, new: &UserRef) -> bool {
        if let Some(member) = self.members.iter_mut().find(|m| m.user.id == id) {
            member.user = new.clone();
            self.sort_members();
            return true;
        }
        false
    }

    fn sort_members(&mut self) {
        self.members.sort_by(|a, b| {
            a.role
                .cmp(&b.role)
                .then_with(|| a.user.name().cmp(b.user.name()))
        });
    }
}

/// Per-backend runtime state the UI needs beyond the buffers themselves.
#[derive(Debug)]
pub struct BackendState {
    pub info: BackendInfo,
    pub nickname: String,
    /// True once the backend has finished delivering initial history (Synced event).
    /// Unread/mention flags are only set after this point so backfill doesn't
    /// trigger activity indicators.
    pub synced: bool,
}

/// Domain state: every backend and buffer, mutated purely by applying
/// [`ChatEvent`]s. Holds no view concerns (focus, mode, scroll position of the
/// active pane) - those live in [`ViewState`].
#[derive(Debug, Default)]
pub struct State {
    pub backends: IndexMap<BackendId, BackendState>,
    pub buffers: IndexMap<BufferId, ChatBuffer>,
}

impl State {
    pub fn new() -> State {
        State::default()
    }

    /// Registers a backend and its status buffer. Called by the main loop as each
    /// backend is spawned.
    pub fn register_backend(&mut self, info: BackendInfo) {
        let id = info.id;
        self.buffers.entry(BufferId::status(id)).or_default();
        self.backends.insert(
            id,
            BackendState {
                info,
                nickname: String::new(),
                synced: false,
            },
        );
    }

    pub fn set_synced(&mut self, backend: BackendId) {
        if let Some(state) = self.backends.get_mut(&backend) {
            state.synced = true;
        }
    }

    pub fn set_nickname(&mut self, backend: BackendId, nickname: String) {
        if let Some(state) = self.backends.get_mut(&backend) {
            state.nickname = nickname;
        }
    }

    pub fn nickname(&self, backend: BackendId) -> &str {
        self.backends
            .get(&backend)
            .map(|state| state.nickname.as_str())
            .unwrap_or_default()
    }

    pub fn focused_buffer_mut<'a>(&'a mut self, view: &ViewState) -> Option<&'a mut ChatBuffer> {
        let focused = view.focused.as_ref()?;
        self.buffers.get_mut(focused)
    }

    fn buffer_mut(&mut self, backend: BackendId, target: TargetId) -> &mut ChatBuffer {
        self.buffers
            .entry(BufferId::new(backend, target))
            .or_default()
    }

    /// Applies a normalized event from `backend`, mutating buffers, rosters, and
    /// topics. This is the single point where domain state changes.
    pub fn apply(&mut self, backend: BackendId, event: ChatEvent) {
        match event {
            ChatEvent::Message { .. } => self.apply_message(backend, event),
            ChatEvent::Edit { target, id, body } => self.apply_edit(backend, target, id, body),
            ChatEvent::Redaction { target, id, .. } => self.apply_redaction(backend, target, id),
            ChatEvent::Reaction {
                target,
                id,
                key,
                add,
                ..
            } => self.apply_reaction(backend, target, id, key, add),
            ChatEvent::Membership { .. } => self.apply_membership(backend, event),
            ChatEvent::Topic {
                ref target,
                ref topic,
                time,
                ..
            } => {
                let target = target.clone();
                let topic = topic.clone();
                let buffer = self.buffer_mut(backend, target);
                buffer.topic = Some(topic);
                buffer.insert_message(StoredMessage::new_at(event, time));
            }
            ChatEvent::BufferName {
                ref target,
                ref name,
            } => {
                let name = name.clone();
                self.buffer_mut(backend, target.clone()).display_name = Some(name);
            }
            ChatEvent::Rename { .. } => self.apply_rename(backend, event),
            ChatEvent::Quit { .. } => self.apply_quit(backend, event),
            ChatEvent::ServerInfo { ref target, .. } => {
                let target = target.clone().unwrap_or_else(TargetId::status);
                self.buffer_mut(backend, target)
                    .insert_message(StoredMessage::new(event, false));
            }
        }
    }

    fn apply_message(&mut self, backend: BackendId, event: ChatEvent) {
        let (target, echo_of, time, id, sender_id, body_text) = match &event {
            ChatEvent::Message {
                target,
                echo_of,
                time,
                id,
                sender,
                body,
                ..
            } => (
                target.clone(),
                *echo_of,
                *time,
                id.clone(),
                sender.id.clone(),
                body.text.clone(),
            ),
            _ => return,
        };

        // Extract before buffer_mut borrows self.
        let own_nick = self.nickname(backend).to_string();
        let is_synced = self.backends.get(&backend).map(|b| b.synced).unwrap_or(false);

        let has_id = id.is_some();
        let buffer = self.buffer_mut(backend, target);

        // Replace the optimistic local echo when the server confirms it, adopting
        // the server timestamp in place of the local send time. The echo was
        // appended at `Local::now()`; the confirmed server time may be earlier
        // (clock skew, or a historical send), so re-insert it at the correct
        // chronological position rather than retiming it in place - leaving it
        // put would break the sorted invariant for every later insert.
        if let Some(txn) = echo_of {
            if let Some(pos) = buffer.messages.iter().position(|m| m.txn() == Some(txn)) {
                let mut slot = buffer.messages.remove(pos);
                slot.event = event;
                slot.pending = false;
                if let Some(time) = time {
                    slot.time = time.with_timezone(&Local);
                }
                buffer.insert_message(slot);
                return;
            }
        }

        // Skip duplicate events (same message re-delivered from both backfill and
        // live sync, or from SDK re-delivery on re-join).
        if let Some(ref event_id) = id {
            if buffer.messages.iter().any(|m| m.has_event_id(event_id)) {
                return;
            }
        }

        // Only the optimistic local echo is pending: it carries a `txn` but no
        // server-assigned id yet. A backfilled or incoming message may also carry
        // a `txn` (our own messages echo their transaction id) but has a real
        // event id, so it is already confirmed.
        let pending = echo_of.is_some() && !has_id;

        // Server time when known (incoming/backfilled); local now otherwise (an
        // optimistic send shows the send time until its echo arrives).
        let mut stored = StoredMessage::new(event, pending);
        if let Some(time) = time {
            stored.time = time.with_timezone(&Local);
        }

        // Insert at the correct chronological position so backfilled messages
        // arriving out of delivery order sort correctly relative to live ones.
        // A pending echo carries `Local::now()`, so this still appends it.
        buffer.insert_message(stored);

        // Set activity flags for real incoming live messages from others.
        // - synced: false means we are still delivering initial history (Matrix backfill),
        //   so skip - history should not trigger unread/mention indicators
        // - pending: own optimistic echo, skip
        // - echo_of.is_some(): confirmed own echo not matched above (rare fallthrough), skip
        // - sender_id == own_nick: server reflection of own message, skip
        if is_synced && !pending && echo_of.is_none() && sender_id != own_nick {
            buffer.has_unread = true;
            if !own_nick.is_empty() {
                let lower_body = body_text.to_lowercase();
                let lower_nick = own_nick.to_lowercase();
                if lower_body.contains(&lower_nick) {
                    buffer.has_mention = true;
                }
            }
        }
    }

    fn apply_edit(&mut self, backend: BackendId, target: TargetId, id: EventId, body: MessageBody) {
        let buffer = self.buffer_mut(backend, target);
        if let Some(slot) = buffer.messages.iter_mut().find(|m| m.has_event_id(&id)) {
            if let ChatEvent::Message { body: existing, .. } = &mut slot.event {
                *existing = body;
            }
        }
    }

    fn apply_redaction(&mut self, backend: BackendId, target: TargetId, id: EventId) {
        let buffer = self.buffer_mut(backend, target);
        if let Some(slot) = buffer.messages.iter_mut().find(|m| m.has_event_id(&id)) {
            slot.redacted = true;
        }
    }

    fn apply_reaction(
        &mut self,
        backend: BackendId,
        target: TargetId,
        id: EventId,
        key: String,
        add: bool,
    ) {
        let buffer = self.buffer_mut(backend, target);
        if let Some(slot) = buffer.messages.iter_mut().find(|m| m.has_event_id(&id)) {
            let count = slot.reactions.entry(key).or_insert(0);
            if add {
                *count += 1;
            } else if *count > 0 {
                *count -= 1;
            }
        }
    }

    fn apply_membership(&mut self, backend: BackendId, event: ChatEvent) {
        let (target, who, change, time) = match &event {
            ChatEvent::Membership {
                target,
                who,
                change,
                time,
            } => (target.clone(), who.clone(), change.clone(), *time),
            _ => return,
        };

        let buffer = self.buffer_mut(backend, target);

        match &change {
            // Roster seeding and role changes update the member list silently.
            MembershipChange::Present { role } => {
                buffer.upsert_member(who, *role);
                return;
            }
            MembershipChange::SetRole { role } => {
                buffer.set_member_role(&who.id, *role);
                return;
            }
            MembershipChange::Join { .. } => {
                // Suppress the line when the member is already present: a
                // re-delivered membership (notably our own, where Matrix may omit
                // `prev_content`) must not produce a phantom "has joined".
                if !buffer.upsert_member(who, MemberRole::Member) {
                    return;
                }
            }
            MembershipChange::Part { .. } | MembershipChange::Kick { .. } => {
                if !buffer.remove_member(&who.id) {
                    return;
                }
            }
            MembershipChange::Invite { .. } => {}
        }

        // Join/Part/Kick/Invite also render an announcement line.
        buffer.insert_message(StoredMessage::new_at(event, time));
    }

    fn apply_rename(&mut self, backend: BackendId, event: ChatEvent) {
        let (who, new_display) = match &event {
            ChatEvent::Rename { who, new_display } => (who.clone(), new_display.clone()),
            _ => return,
        };

        let renamed = UserRef::new(new_display.clone());

        if self.nickname(backend) == who.id {
            self.set_nickname(backend, new_display.clone());
        }

        self.for_backend_buffers(backend, |buffer| {
            if buffer.rename_member(&who.id, &renamed) {
                buffer.insert_message(StoredMessage::new(event.clone(), false));
            }
        });
    }

    fn apply_quit(&mut self, backend: BackendId, event: ChatEvent) {
        let who = match &event {
            ChatEvent::Quit { who, .. } => who.clone(),
            _ => return,
        };

        self.for_backend_buffers(backend, |buffer| {
            if buffer.remove_member(&who.id) {
                buffer.insert_message(StoredMessage::new(event.clone(), false));
            }
        });
    }

    fn for_backend_buffers(&mut self, backend: BackendId, mut f: impl FnMut(&mut ChatBuffer)) {
        for (id, buffer) in self.buffers.iter_mut() {
            if id.backend == backend {
                f(buffer);
            }
        }
    }
}

/// View-layer state: which buffer is focused and the input mode. Kept separate
/// from domain [`State`] so a future split-pane layout can track focus/scroll
/// per pane without touching the domain model.
#[derive(Debug, Default)]
pub struct ViewState {
    pub mode: Mode,
    pub focused: Option<BufferId>,
    /// Height of the message area from the most recent render (terminal rows).
    /// Updated by the renderer after each draw; used by scroll key handlers to
    /// compute page-height steps without a second terminal size query.
    pub viewport_height: u16,
}

impl ViewState {
    pub fn new() -> Self {
        ViewState::default()
    }

    /// Focuses `buffer` if nothing is focused yet (e.g. the first backend's
    /// status buffer at startup).
    pub fn focus_if_unset(&mut self, buffer: BufferId) {
        if self.focused.is_none() {
            self.focused = Some(buffer);
        }
    }

    pub fn focus(&mut self, buffer: BufferId) {
        self.focused = Some(buffer);
    }

    fn focused_index(&self, state: &State) -> Option<usize> {
        let focused = self.focused.as_ref()?;
        state.buffers.keys().position(|id| id == focused)
    }

    fn focus_index(&mut self, state: &State, index: usize) {
        if let Some((id, _)) = state.buffers.get_index(index) {
            self.focused = Some(id.clone());
        }
    }

    pub fn next_buffer(&mut self, state: &State) {
        if state.buffers.is_empty() {
            return;
        }
        let current = self.focused_index(state).unwrap_or(0);
        self.focus_index(state, (current + 1) % state.buffers.len());
    }

    pub fn previous_buffer(&mut self, state: &State) {
        if state.buffers.is_empty() {
            return;
        }
        let len = state.buffers.len();
        let current = self.focused_index(state).unwrap_or(0);
        self.focus_index(state, (current + len - 1) % len);
    }

    pub fn focus_buffer_index(&mut self, state: &State, index: usize) {
        if index < state.buffers.len() {
            self.focus_index(state, index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{MsgKind, Protocol};

    fn backend() -> BackendId {
        BackendId(0)
    }

    fn test_state() -> State {
        let mut state = State::new();
        state.register_backend(BackendInfo {
            id: backend(),
            protocol: Protocol::Irc,
            name: "test".to_string(),
        });
        state.set_nickname(backend(), "me".to_string());
        state
    }

    fn message(target: &str, sender: &str, echo_of: Option<TxnId>) -> ChatEvent {
        ChatEvent::Message {
            target: TargetId::from(target),
            id: None,
            sender: UserRef::new(sender),
            body: MessageBody::plain("hi"),
            kind: MsgKind::Text,
            echo_of,
            time: None,
        }
    }

    fn buffer<'a>(state: &'a State, target: &str) -> &'a ChatBuffer {
        state
            .buffers
            .get(&BufferId::new(backend(), target))
            .expect("buffer exists")
    }

    #[test]
    fn status_buffer_is_created_for_backend() {
        let state = test_state();
        assert!(state.buffers.contains_key(&BufferId::status(backend())));
    }

    #[test]
    fn incoming_message_is_appended() {
        let mut state = test_state();
        state.apply(backend(), message("#tirc", "alice", None));
        assert_eq!(buffer(&state, "#tirc").messages.len(), 1);
        assert!(!buffer(&state, "#tirc").messages[0].pending);
    }

    #[test]
    fn optimistic_echo_is_replaced_in_place() {
        let mut state = test_state();
        let txn = TxnId(7);

        // Optimistic local copy.
        state.apply(backend(), message("#tirc", "me", Some(txn)));
        assert_eq!(buffer(&state, "#tirc").messages.len(), 1);
        assert!(buffer(&state, "#tirc").messages[0].pending);

        // Server echo with the same txn replaces it rather than duplicating.
        state.apply(backend(), message("#tirc", "me", Some(txn)));
        assert_eq!(buffer(&state, "#tirc").messages.len(), 1);
        assert!(!buffer(&state, "#tirc").messages[0].pending);
    }

    #[test]
    fn backfilled_own_message_with_txn_is_not_pending() {
        let mut state = test_state();

        // A backfilled message we sent: it echoes its transaction id, but it has
        // a real event id, so it must render confirmed (not dimmed).
        state.apply(
            backend(),
            ChatEvent::Message {
                target: TargetId::from("#tirc"),
                id: Some(crate::core::EventId("$evt".to_string())),
                sender: UserRef::new("me"),
                body: MessageBody::plain("old message"),
                kind: MsgKind::Text,
                echo_of: Some(TxnId(3)),
                time: None,
            },
        );

        let buffer = buffer(&state, "#tirc");
        assert_eq!(buffer.messages.len(), 1);
        assert!(!buffer.messages[0].pending);
    }

    #[test]
    fn optimistic_send_uses_send_time_then_adopts_server_time() {
        let mut state = test_state();
        let txn = TxnId(11);

        // Optimistic copy carries no server time, so it shows the local send time.
        let before = Local::now();
        state.apply(backend(), message("#tirc", "me", Some(txn)));
        let optimistic_time = buffer(&state, "#tirc").messages[0].time;
        assert!(optimistic_time >= before);

        // The confirmed echo carries the server timestamp, which replaces it.
        let server_time = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        state.apply(
            backend(),
            ChatEvent::Message {
                target: TargetId::from("#tirc"),
                id: None,
                sender: UserRef::new("me"),
                body: MessageBody::plain("hi"),
                kind: MsgKind::Text,
                echo_of: Some(txn),
                time: Some(server_time),
            },
        );

        let buffer = buffer(&state, "#tirc");
        assert_eq!(buffer.messages.len(), 1);
        assert_eq!(buffer.messages[0].time, server_time.with_timezone(&Local));
    }

    #[test]
    fn join_updates_roster_and_renders_line() {
        let mut state = test_state();
        state.apply(
            backend(),
            ChatEvent::Membership {
                target: TargetId::from("#tirc"),
                who: UserRef::new("alice"),
                change: MembershipChange::Join { realname: None },
                time: None,
            },
        );
        let buffer = buffer(&state, "#tirc");
        assert_eq!(buffer.members.len(), 1);
        assert_eq!(buffer.messages.len(), 1);
    }

    #[test]
    fn present_seeds_roster_silently_and_sorts_by_role() {
        let mut state = test_state();
        for (nick, role) in [("carol", MemberRole::Member), ("alice", MemberRole::Op)] {
            state.apply(
                backend(),
                ChatEvent::Membership {
                    target: TargetId::from("#tirc"),
                    who: UserRef::new(nick),
                    change: MembershipChange::Present { role },
                    time: None,
                },
            );
        }
        let buffer = buffer(&state, "#tirc");
        assert_eq!(buffer.messages.len(), 0, "roster seeding renders no line");
        assert_eq!(buffer.members[0].user.id, "alice", "op sorts first");
        assert_eq!(buffer.members[1].user.id, "carol");
    }

    #[test]
    fn rename_updates_roster_across_buffer() {
        let mut state = test_state();
        state.apply(
            backend(),
            ChatEvent::Membership {
                target: TargetId::from("#tirc"),
                who: UserRef::new("alice"),
                change: MembershipChange::Join { realname: None },
                time: None,
            },
        );
        state.apply(
            backend(),
            ChatEvent::Rename {
                who: UserRef::new("alice"),
                new_display: "alice2".to_string(),
            },
        );
        let buffer = buffer(&state, "#tirc");
        assert!(buffer.members.iter().any(|m| m.user.id == "alice2"));
        assert!(!buffer.members.iter().any(|m| m.user.id == "alice"));
    }

    #[test]
    fn view_navigation_wraps_over_buffers() {
        let mut state = test_state();
        state.apply(backend(), message("#a", "x", None));
        state.apply(backend(), message("#b", "y", None));

        let mut view = ViewState::new();
        view.focus(BufferId::status(backend()));

        // status, #a, #b -> 3 buffers
        assert_eq!(state.buffers.len(), 3);
        view.next_buffer(&state);
        assert_eq!(view.focused.as_ref().unwrap().target.as_str(), "#a");
        view.previous_buffer(&state);
        assert!(view.focused.as_ref().unwrap().target.is_status());
    }

    fn buffer_with_messages(n: usize) -> ChatBuffer {
        let mut buf = ChatBuffer::default();
        for i in 0..n {
            let event = ChatEvent::Message {
                target: TargetId::from("#test"),
                id: None,
                sender: UserRef::new("alice"),
                body: MessageBody::plain(format!("msg {i}")),
                kind: MsgKind::Text,
                echo_of: None,
                time: None,
            };
            buf.messages.push(StoredMessage::new(event, false));
        }
        buf
    }

    #[test]
    fn scroll_up_clamps_at_last_message() {
        let mut buf = buffer_with_messages(10);
        buf.scroll_up(5);
        assert_eq!(buf.scroll_position, 5);
        buf.scroll_up(100);
        // max is messages.len() - 1 = 9
        assert_eq!(buf.scroll_position, 9);
    }

    #[test]
    fn scroll_down_clamps_at_zero() {
        let mut buf = buffer_with_messages(10);
        buf.scroll_position = 5;
        buf.scroll_down(3);
        assert_eq!(buf.scroll_position, 2);
        buf.scroll_down(100);
        assert_eq!(buf.scroll_position, 0);
    }

    #[test]
    fn scroll_to_top_leaves_viewport_height_messages_visible() {
        let mut buf = buffer_with_messages(20);
        buf.scroll_to_top(10);
        // With 20 messages and viewport 10, scroll_position should be 10
        // so that messages [0..10] (oldest) fill the screen.
        assert_eq!(buf.scroll_position, 10);
    }

    #[test]
    fn scroll_to_bottom_returns_to_tail() {
        let mut buf = buffer_with_messages(10);
        buf.scroll_position = 7;
        buf.scroll_to_bottom();
        assert_eq!(buf.scroll_position, 0);
    }

    #[test]
    fn scroll_on_empty_buffer_is_safe() {
        let mut buf = ChatBuffer::default();
        buf.scroll_up(5);
        assert_eq!(buf.scroll_position, 0);
        buf.scroll_down(5);
        assert_eq!(buf.scroll_position, 0);
        buf.scroll_to_top(10);
        assert_eq!(buf.scroll_position, 0);
    }

    fn timed_message(target: &str, sender: &str, event_id: &str, ts: i64) -> ChatEvent {
        ChatEvent::Message {
            target: TargetId::from(target),
            id: Some(crate::core::EventId(event_id.to_string())),
            sender: UserRef::new(sender),
            body: MessageBody::plain("hi"),
            kind: MsgKind::Text,
            echo_of: None,
            time: chrono::DateTime::from_timestamp(ts, 0),
        }
    }

    #[test]
    fn backfill_messages_are_sorted_chronologically() {
        let mut state = test_state();

        // Deliver oldest message last (as backfill often does).
        state.apply(backend(), timed_message("#tirc", "alice", "$b", 2000));
        state.apply(backend(), timed_message("#tirc", "bob", "$a", 1000));

        let buf = buffer(&state, "#tirc");
        assert_eq!(buf.messages.len(), 2);
        assert_eq!(
            buf.messages[0].time.timestamp(),
            1000,
            "older message must be first"
        );
        assert_eq!(buf.messages[1].time.timestamp(), 2000);
    }

    #[test]
    fn duplicate_event_id_is_skipped() {
        let mut state = test_state();

        state.apply(backend(), timed_message("#tirc", "alice", "$x", 1000));
        // Same event ID arrives again (e.g., from live sync after backfill).
        state.apply(backend(), timed_message("#tirc", "alice", "$x", 1000));

        let buf = buffer(&state, "#tirc");
        assert_eq!(buf.messages.len(), 1, "duplicate must not be inserted");
    }

    #[test]
    fn pending_echo_still_appends_to_end() {
        let mut state = test_state();
        let txn = TxnId(42);

        state.apply(backend(), timed_message("#tirc", "alice", "$old", 1000));
        // Optimistic echo has no event ID and no server time.
        state.apply(backend(), message("#tirc", "me", Some(txn)));

        let buf = buffer(&state, "#tirc");
        assert_eq!(buf.messages.len(), 2);
        assert!(buf.messages[1].pending, "pending echo must be at the end");
    }

    #[test]
    fn confirmed_echo_is_repositioned_by_server_time() {
        let mut state = test_state();
        let txn = TxnId(5);

        // A backfilled history message sits in the buffer.
        state.apply(backend(), timed_message("#tirc", "alice", "$hist", 2000));

        // We send a message: the optimistic echo appends at `Local::now()`, far
        // ahead of the history timestamp.
        state.apply(backend(), message("#tirc", "me", Some(txn)));
        assert!(buffer(&state, "#tirc").messages[1].pending);

        // The server confirms it with an *earlier* timestamp (1000 < 2000), so it
        // belongs before the history message. An in-place retime would leave it
        // stranded at the tail and break sortedness for later inserts.
        let server_time = chrono::DateTime::from_timestamp(1000, 0).unwrap();
        state.apply(
            backend(),
            ChatEvent::Message {
                target: TargetId::from("#tirc"),
                id: Some(crate::core::EventId("$echo".to_string())),
                sender: UserRef::new("me"),
                body: MessageBody::plain("hi"),
                kind: MsgKind::Text,
                echo_of: Some(txn),
                time: Some(server_time),
            },
        );

        let buf = buffer(&state, "#tirc");
        assert_eq!(buf.messages.len(), 2);
        assert_eq!(
            buf.messages[0].time.timestamp(),
            1000,
            "confirmed echo must move to its chronological position"
        );
        assert_eq!(buf.messages[1].time.timestamp(), 2000);
    }

    #[test]
    fn join_for_already_present_member_renders_no_line() {
        let mut state = test_state();

        // Roster is seeded (e.g. on startup) with alice already present.
        state.apply(
            backend(),
            ChatEvent::Membership {
                target: TargetId::from("#tirc"),
                who: UserRef::new("alice"),
                change: MembershipChange::Present {
                    role: MemberRole::Member,
                },
                time: None,
            },
        );

        // A re-delivered membership for alice (e.g. our own event with no
        // `prev_content`) must not produce a phantom "has joined" line.
        state.apply(
            backend(),
            ChatEvent::Membership {
                target: TargetId::from("#tirc"),
                who: UserRef::new("alice"),
                change: MembershipChange::Join { realname: None },
                time: None,
            },
        );

        let buffer = buffer(&state, "#tirc");
        assert_eq!(buffer.members.len(), 1);
        assert_eq!(buffer.messages.len(), 0, "redundant join renders no line");
    }

    #[test]
    fn membership_uses_server_time_for_ordering() {
        let mut state = test_state();

        // A later message is already in the buffer.
        state.apply(backend(), timed_message("#tirc", "alice", "$later", 2000));

        // A join carrying an *earlier* server time must sort before it rather than
        // being stamped with `Local::now()` and appended at the tail.
        state.apply(
            backend(),
            ChatEvent::Membership {
                target: TargetId::from("#tirc"),
                who: UserRef::new("bob"),
                change: MembershipChange::Join { realname: None },
                time: chrono::DateTime::from_timestamp(1000, 0),
            },
        );

        let buf = buffer(&state, "#tirc");
        assert_eq!(buf.messages.len(), 2);
        assert!(
            matches!(buf.messages[0].event, ChatEvent::Membership { .. }),
            "earlier-dated join sorts before the later message"
        );
        assert_eq!(buf.messages[0].time.timestamp(), 1000);
        assert_eq!(buf.messages[1].time.timestamp(), 2000);
    }

    #[test]
    fn membership_line_sorts_by_time_among_messages() {
        let mut state = test_state();

        // A message timestamped in the far future (year ~2065) is already in the
        // buffer.
        state.apply(
            backend(),
            timed_message("#tirc", "alice", "$future", 3_000_000_000),
        );

        // A live "has joined" line is rendered with `Local::now()`, which is
        // *earlier* than the future message. It must sort before that message
        // rather than being appended at the tail (a direct `push` would leave the
        // buffer unsorted: [future, join]).
        state.apply(
            backend(),
            ChatEvent::Membership {
                target: TargetId::from("#tirc"),
                who: UserRef::new("bob"),
                change: MembershipChange::Join { realname: None },
                time: None,
            },
        );

        let buf = buffer(&state, "#tirc");
        assert_eq!(buf.messages.len(), 2);
        assert!(
            matches!(buf.messages[0].event, ChatEvent::Membership { .. }),
            "join line (now) sorts before the future-dated message"
        );
        assert!(matches!(buf.messages[1].event, ChatEvent::Message { .. }));
    }
}
