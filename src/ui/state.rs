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
    fn upsert_member(&mut self, user: UserRef, role: MemberRole) {
        match self.members.iter_mut().find(|m| m.user.id == user.id) {
            Some(member) => {
                member.user = user;
                member.role = role;
            }
            None => self.members.push(Member { user, role }),
        }
        self.sort_members();
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
            },
        );
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
                ..
            } => {
                let target = target.clone();
                let topic = topic.clone();
                let buffer = self.buffer_mut(backend, target);
                buffer.topic = Some(topic);
                buffer.messages.push(StoredMessage::new(event, false));
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
                    .messages
                    .push(StoredMessage::new(event, false));
            }
        }
    }

    fn apply_message(&mut self, backend: BackendId, event: ChatEvent) {
        let (target, echo_of, time, has_id) = match &event {
            ChatEvent::Message {
                target,
                echo_of,
                time,
                id,
                ..
            } => (target.clone(), *echo_of, *time, id.is_some()),
            _ => return,
        };

        let buffer = self.buffer_mut(backend, target);

        // Replace the optimistic local echo in place when the server confirms it,
        // adopting the server timestamp in place of the local send time.
        if let Some(txn) = echo_of {
            if let Some(slot) = buffer.messages.iter_mut().find(|m| m.txn() == Some(txn)) {
                slot.event = event;
                slot.pending = false;
                if let Some(time) = time {
                    slot.time = time.with_timezone(&Local);
                }
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
        buffer.messages.push(stored);
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
        let (target, who, change) = match &event {
            ChatEvent::Membership {
                target,
                who,
                change,
            } => (target.clone(), who.clone(), change.clone()),
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
            MembershipChange::Join { .. } => buffer.upsert_member(who, MemberRole::Member),
            MembershipChange::Part { .. } | MembershipChange::Kick { .. } => {
                buffer.remove_member(&who.id);
            }
            MembershipChange::Invite { .. } => {}
        }

        // Join/Part/Kick/Invite also render an announcement line.
        buffer.messages.push(StoredMessage::new(event, false));
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
                buffer
                    .messages
                    .push(StoredMessage::new(event.clone(), false));
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
                buffer
                    .messages
                    .push(StoredMessage::new(event.clone(), false));
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
}
