use std::ops::RangeInclusive;

use chrono::{DateTime, Local};
use indexmap::IndexMap;
use ratatui::layout::Rect;

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
    /// True when the message body was replaced by an incoming edit event.
    pub edited: bool,
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
            edited: false,
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
        let is_synced = self
            .backends
            .get(&backend)
            .map(|b| b.synced)
            .unwrap_or(false);

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
                slot.edited = true;
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

/// A map of on-screen hit regions the renderer fills in each frame and the
/// input handler reads to turn mouse coordinates into actions. It mirrors the
/// existing precedent where the renderer writes [`ViewState::viewport_height`]:
/// geometry is recomputed per frame and the latest copy lives here so input
/// handling does not need to re-derive the layout.
///
/// Only the first row of the buffer bar is mapped (`bar_tabs`), which matches
/// the default theme's single-row layout exactly. Multi-row or separator-heavy
/// custom themes (e.g. `slanted`) degrade to approximate boxes, where a misclick
/// lands on an adjacent buffer - low harm.
#[derive(Debug, Default, Clone)]
pub struct LayoutMap {
    /// The message area (excludes the user list when a sidebar is shown).
    pub message_rect: Rect,
    /// The whole buffer bar region.
    pub bar_rect: Rect,
    /// Per-tab hit boxes paired with the buffer they select, left to right in
    /// buffer order. Built by measuring each tab's rendered display width.
    pub bar_tabs: Vec<(Rect, BufferId)>,
    /// The user list region, or `None` when the sidebar is hidden.
    pub userlist_rect: Option<Rect>,
    /// Index of the first member rendered in the user list. Always 0 today;
    /// reserved for a future scrollable user list so `member_row_at` can map a
    /// screen row back to the correct roster index.
    pub userlist_first_member: usize,
    /// Column of the sidebar split boundary, or `None` when there is no sidebar.
    /// Reserved for the resizable-sidebar drag handling in a later commit.
    pub split_x: Option<u16>,
}

impl LayoutMap {
    /// Returns the buffer whose tab hit box contains `(x, y)`, if any. Used to
    /// turn a left-click on the buffer bar into a focus switch.
    pub fn tab_at(&self, x: u16, y: u16) -> Option<&BufferId> {
        self.bar_tabs
            .iter()
            .find_map(|(rect, id)| rect_contains(rect, x, y).then_some(id))
    }

    /// Returns the member index (relative to `userlist_first_member`) for a
    /// click at `(x, y)` within the user list. The list is drawn in a `Block`
    /// with a title, which ratatui places on the block's first row, so the first
    /// member sits at `rect.y + 1`. Returns `None` when there is no user list,
    /// the click falls outside it, or it lands on the title row.
    pub fn member_row_at(&self, x: u16, y: u16) -> Option<usize> {
        let rect = self.userlist_rect?;
        if !rect_contains(&rect, x, y) {
            return None;
        }
        let first_member_row = rect.y.checked_add(1)?;
        if y < first_member_row {
            return None;
        }
        Some(self.userlist_first_member + (y - first_member_row) as usize)
    }
}

/// Whether `(x, y)` falls inside `rect`. ratatui exposes `Rect::contains` only
/// via a `Position`, so this small helper keeps the hit-test call sites terse.
fn rect_contains(rect: &Rect, x: u16, y: u16) -> bool {
    x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

/// What a right-click context menu acts on. Kept as plain data with no command
/// coupling: the input handler translates a [`MenuAction`] plus the target into
/// a backend [`Command`](crate::core::Command) or a local state mutation, so the
/// view layer stays free of protocol concerns.
#[derive(Debug, Clone)]
pub enum MenuTarget {
    /// A buffer-bar tab: the menu acts on this buffer.
    Buffer(BufferId),
    /// A user-list row: the menu acts on this member of the focused buffer.
    User { backend: BackendId, nick: String },
}

/// The abstract action a menu item performs. Deliberately decoupled from
/// [`Command`](crate::core::Command): the input handler owns the mapping from an
/// action to its concrete effect, which keeps `ViewState` testable without a
/// backend and lets the same action mean different things per target type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    /// Close (locally remove) a buffer without leaving the channel.
    CloseBuffer,
    /// Clear a buffer's unread/mention indicators.
    MarkRead,
    /// Leave a channel (sends a Part).
    Leave,
    /// Run a whois on a user.
    Whois,
    /// Open or focus a query buffer with a user.
    OpenQuery,
    /// Insert a `nick: ` mention into the input line.
    Mention,
}

/// One selectable row in a [`ContextMenu`].
#[derive(Debug, Clone)]
pub struct MenuItem {
    pub label: String,
    pub action: MenuAction,
}

/// A floating right-click context menu, shared by the buffer bar and the user
/// list. Holds only plain data; the renderer draws it last (on top of the frame)
/// and writes back the resolved on-screen [`rect`](ContextMenu::rect) so the
/// input handler can hit-test clicks against the same geometry that was drawn,
/// rather than re-deriving the clamp.
#[derive(Debug, Clone, Default)]
pub struct ContextMenu {
    /// Whether the menu is currently shown and capturing input modally.
    pub open: bool,
    /// Anchor column where the menu was opened (the click position). The renderer
    /// clamps this so the menu stays fully on-screen.
    pub x: u16,
    /// Anchor row where the menu was opened. Clamped like [`Self::x`].
    pub y: u16,
    /// Index of the highlighted item.
    pub selected: usize,
    pub items: Vec<MenuItem>,
    /// What the menu acts on, or `None` before it is opened.
    pub target: Option<MenuTarget>,
    /// Resolved on-screen rectangle from the most recent render, used by the
    /// input handler to hit-test clicks. Written by the renderer each frame the
    /// menu is open; meaningless while closed.
    pub rect: Rect,
}

impl ContextMenu {
    /// Opens the menu at `(x, y)` for `target` with `items`, resetting the
    /// selection to the first row.
    pub fn open_at(&mut self, x: u16, y: u16, target: MenuTarget, items: Vec<MenuItem>) {
        self.open = true;
        self.x = x;
        self.y = y;
        self.selected = 0;
        self.items = items;
        self.target = Some(target);
    }

    /// Closes the menu and drops its items/target so a stale target can never be
    /// activated after the fact.
    pub fn close(&mut self) {
        self.open = false;
        self.items.clear();
        self.target = None;
        self.selected = 0;
    }

    /// Moves the highlight up one row, clamped at the top (no wrap).
    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Moves the highlight down one row, clamped at the last item (no wrap).
    pub fn move_down(&mut self) {
        let last = self.items.len().saturating_sub(1);
        self.selected = (self.selected + 1).min(last);
    }

    /// The action of the highlighted item, or `None` when the menu is empty.
    pub fn selected_action(&self) -> Option<MenuAction> {
        self.items.get(self.selected).map(|item| item.action)
    }

    /// The on-screen rectangle the menu occupies within `area`, clamped so it is
    /// always fully visible. One rule handles both the bottom bar and the
    /// sidebar: anchoring near an edge pushes the box back inward (a bar menu near
    /// `area.height` slides upward). The renderer uses this to draw the menu and
    /// stores the result in [`Self::rect`] for the input handler to read back.
    pub fn resolved_rect(&self, area: Rect) -> Rect {
        let longest = self
            .items
            .iter()
            .map(|item| item.label.chars().count())
            .max()
            .unwrap_or(0) as u16;
        // Two columns of padding around the label plus two for the borders.
        let menu_w = longest.saturating_add(4).min(area.width.max(1));
        // One row per item plus the top and bottom borders.
        let menu_h = (self.items.len() as u16)
            .saturating_add(2)
            .min(area.height.max(1));
        let x = self.x.min(area.width.saturating_sub(menu_w));
        let y = self.y.min(area.height.saturating_sub(menu_h));
        Rect {
            x,
            y,
            width: menu_w,
            height: menu_h,
        }
    }

    /// Whether `(x, y)` lands inside the menu's last-rendered rectangle.
    pub fn contains(&self, x: u16, y: u16) -> bool {
        self.open && rect_contains(&self.rect, x, y)
    }

    /// The item index for a click at `(x, y)`, accounting for the top border, or
    /// `None` when the click is outside the menu or on its border.
    pub fn item_at(&self, x: u16, y: u16) -> Option<usize> {
        if !self.contains(x, y) {
            return None;
        }
        let first_row = self.rect.y.checked_add(1)?;
        if y < first_row {
            return None;
        }
        let index = (y - first_row) as usize;
        (index < self.items.len()).then_some(index)
    }
}

/// An in-progress or completed text selection over the message area, in
/// terminal cell coordinates. `anchor` is where the drag began (the fixed end);
/// `cursor` is the moving end that follows the pointer. v1 is line-granular: the
/// whole width of the message area is selected for every row the selection
/// spans, so only the row of each end matters for what gets copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Where the drag started (fixed end). `(column, row)`.
    pub anchor: (u16, u16),
    /// Where the pointer currently is (moving end). `(column, row)`.
    pub cursor: (u16, u16),
}

impl Selection {
    /// A fresh zero-length selection anchored at `(x, y)`.
    pub fn new(x: u16, y: u16) -> Self {
        Selection {
            anchor: (x, y),
            cursor: (x, y),
        }
    }

    /// The inclusive range of rows the selection covers, ordered low to high so
    /// it is valid regardless of whether the drag went upward or downward. Used
    /// both to highlight cells and to read the selected text back out of the
    /// rendered frame.
    pub fn selected_rows(&self) -> RangeInclusive<u16> {
        let a = self.anchor.1;
        let b = self.cursor.1;
        a.min(b)..=a.max(b)
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
    /// Horizontal scroll offset of the buffer bar (columns). Persisted across
    /// frames so "follow" mode can scroll minimally without jumping each frame.
    pub bar_x_scroll: u16,
    /// Hit-region map from the most recent render, read by the input handler to
    /// resolve mouse clicks to buffers and user-list members.
    pub layout: LayoutMap,
    /// User-set width of the user-list sidebar in columns, or `None` to use the
    /// default proportional width. Driven by the resize keybinds and the split
    /// drag; clamped to a usable range by [`ViewState::sidebar_constraint_width`]
    /// at render time so the stored value never has to be valid on its own.
    pub sidebar_width: Option<u16>,
    /// The right-click context menu. Closed by default; opened by a right-click
    /// on a buffer tab or user row and drawn on top of the frame by the renderer.
    pub menu: ContextMenu,
    /// True while "copy mode" is active: the app has released mouse capture so
    /// the terminal performs its own native selection. Read by the renderer to
    /// show a `-- COPY --` hint; toggled by a keybind that flips terminal mouse
    /// capture on the side.
    pub copy_mode: bool,
    /// The current app-level message-area selection, or `None` when nothing is
    /// selected. Set on a left drag in the message area, highlighted by the
    /// renderer, and read by the yank keybind to copy text to the clipboard.
    pub selection: Option<Selection>,
}

/// Minimum sidebar width in columns. Narrow enough for short nicks while still
/// leaving room for the list border.
const SIDEBAR_MIN_WIDTH: u16 = 8;
/// Columns reserved for the message area, so a wide sidebar can never collapse
/// the conversation it sits beside.
const SIDEBAR_MESSAGE_RESERVE: u16 = 20;

impl ViewState {
    pub fn new() -> Self {
        ViewState::default()
    }

    /// The sidebar width to use for a horizontal split of `total_width` columns.
    ///
    /// With an explicit [`ViewState::sidebar_width`] the value is clamped to
    /// `[SIDEBAR_MIN_WIDTH, total_width - SIDEBAR_MESSAGE_RESERVE]` so the message
    /// area keeps a usable minimum. On a terminal too narrow to honour both
    /// reservations the message reserve wins and whatever is left goes to the
    /// sidebar - clamping here (rather than via `u16::clamp`, which panics when
    /// `min > max`) keeps that degenerate case from crashing. Without an explicit
    /// width it falls back to the historical 10% proportional split.
    pub fn sidebar_constraint_width(&self, total_width: u16) -> u16 {
        match self.sidebar_width {
            Some(width) => {
                let max = total_width.saturating_sub(SIDEBAR_MESSAGE_RESERVE);
                if max < SIDEBAR_MIN_WIDTH {
                    return max;
                }
                width.clamp(SIDEBAR_MIN_WIDTH, max)
            }
            None => total_width * 10 / 100,
        }
    }

    /// Widens the sidebar by `step` columns, seeding from the width actually
    /// rendered last frame so the change stays responsive even when the stored
    /// value would otherwise run past the clamp. The renderer re-clamps on draw.
    pub fn grow_sidebar(&mut self, step: u16) {
        self.sidebar_width = Some(self.current_sidebar_width().saturating_add(step));
    }

    /// Narrows the sidebar by `step` columns. Symmetric with [`Self::grow_sidebar`].
    pub fn shrink_sidebar(&mut self, step: u16) {
        self.sidebar_width = Some(self.current_sidebar_width().saturating_sub(step));
    }

    /// Clears the override so the sidebar returns to its default proportional width.
    pub fn reset_sidebar_width(&mut self) {
        self.sidebar_width = None;
    }

    /// The sidebar width to grow/shrink from: the last rendered width (already
    /// clamped) when a sidebar was shown, else the stored override, else the
    /// minimum. Using the rendered width keeps the keybinds responsive at the
    /// clamp boundaries.
    fn current_sidebar_width(&self) -> u16 {
        self.layout
            .userlist_rect
            .map(|rect| rect.width)
            .or(self.sidebar_width)
            .unwrap_or(SIDEBAR_MIN_WIDTH)
    }

    /// Flips copy mode and returns the new value. The caller pairs this with the
    /// terminal mouse-capture toggle (which lives on the [`Tui`](crate::tui::Tui)
    /// since it touches the backend), so the flag here only drives the renderer's
    /// hint. Leaving copy mode also drops any app-level selection, which is
    /// meaningless once native selection has taken over.
    pub fn toggle_copy_mode(&mut self) -> bool {
        self.copy_mode = !self.copy_mode;
        if self.copy_mode {
            self.selection = None;
        }
        self.copy_mode
    }

    /// Drops the current selection, if any. Used after a yank and when a click
    /// starts somewhere that should not extend the previous selection.
    pub fn clear_selection(&mut self) {
        self.selection = None;
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
    fn tab_at_resolves_clicks_to_contiguous_hit_boxes() {
        let a = BufferId::new(backend(), "#a");
        let b = BufferId::new(backend(), "#b");
        let layout = LayoutMap {
            bar_tabs: vec![
                (
                    Rect {
                        x: 0,
                        y: 0,
                        width: 5,
                        height: 1,
                    },
                    a.clone(),
                ),
                (
                    Rect {
                        x: 5,
                        y: 0,
                        width: 4,
                        height: 1,
                    },
                    b.clone(),
                ),
            ],
            ..LayoutMap::default()
        };

        assert_eq!(layout.tab_at(0, 0), Some(&a));
        assert_eq!(layout.tab_at(4, 0), Some(&a));
        assert_eq!(
            layout.tab_at(5, 0),
            Some(&b),
            "boundary belongs to next tab"
        );
        assert_eq!(layout.tab_at(8, 0), Some(&b));
        assert_eq!(layout.tab_at(9, 0), None, "past the last tab");
        assert_eq!(layout.tab_at(0, 1), None, "wrong row");
    }

    #[test]
    fn member_row_at_accounts_for_title_offset() {
        let layout = LayoutMap {
            userlist_rect: Some(Rect {
                x: 80,
                y: 0,
                width: 10,
                height: 5,
            }),
            ..LayoutMap::default()
        };

        assert_eq!(
            layout.member_row_at(85, 0),
            None,
            "title row is not a member"
        );
        assert_eq!(layout.member_row_at(85, 1), Some(0), "first member row");
        assert_eq!(layout.member_row_at(85, 2), Some(1));
        assert_eq!(layout.member_row_at(85, 4), Some(3), "last visible row");
        assert_eq!(layout.member_row_at(79, 1), None, "left of the list");
        assert_eq!(layout.member_row_at(90, 1), None, "right of the list");
        assert_eq!(layout.member_row_at(85, 5), None, "below the list");
    }

    #[test]
    fn sidebar_width_defaults_to_ten_percent() {
        let view = ViewState::new();
        assert_eq!(view.sidebar_constraint_width(100), 10);
        assert_eq!(view.sidebar_constraint_width(80), 8);
    }

    #[test]
    fn sidebar_width_override_is_clamped_to_min_and_max() {
        let mut view = ViewState::new();

        // Below the minimum is raised to the floor.
        view.sidebar_width = Some(1);
        assert_eq!(view.sidebar_constraint_width(100), SIDEBAR_MIN_WIDTH);

        // Above the max (total - message reserve) is capped.
        view.sidebar_width = Some(200);
        assert_eq!(
            view.sidebar_constraint_width(100),
            100 - SIDEBAR_MESSAGE_RESERVE
        );

        // A value in range passes through unchanged.
        view.sidebar_width = Some(30);
        assert_eq!(view.sidebar_constraint_width(100), 30);
    }

    #[test]
    fn sidebar_width_on_narrow_terminal_does_not_panic() {
        let mut view = ViewState::new();
        view.sidebar_width = Some(50);
        // total_width - reserve (20) is below the min (8), so the message reserve
        // wins and the sidebar gets whatever is left rather than panicking on a
        // `min > max` clamp.
        assert_eq!(view.sidebar_constraint_width(20), 0);
        assert_eq!(view.sidebar_constraint_width(25), 5);
    }

    #[test]
    fn grow_and_shrink_seed_from_rendered_width() {
        let mut view = ViewState::new();
        view.layout.userlist_rect = Some(Rect {
            x: 90,
            y: 0,
            width: 10,
            height: 5,
        });

        view.grow_sidebar(2);
        assert_eq!(view.sidebar_width, Some(12));

        view.shrink_sidebar(4);
        // Seeds from the rendered width (10) again, not the stored 12.
        assert_eq!(view.sidebar_width, Some(6));

        view.reset_sidebar_width();
        assert_eq!(view.sidebar_width, None);
    }

    #[test]
    fn member_row_at_without_userlist_is_none() {
        let layout = LayoutMap::default();
        assert_eq!(layout.member_row_at(0, 0), None);
    }

    #[test]
    fn member_row_at_applies_first_member_offset() {
        let layout = LayoutMap {
            userlist_rect: Some(Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 5,
            }),
            userlist_first_member: 3,
            ..LayoutMap::default()
        };
        assert_eq!(layout.member_row_at(2, 1), Some(3));
        assert_eq!(layout.member_row_at(2, 2), Some(4));
    }

    fn menu_items() -> Vec<MenuItem> {
        vec![
            MenuItem {
                label: "Mark read".to_string(),
                action: MenuAction::MarkRead,
            },
            MenuItem {
                label: "Leave".to_string(),
                action: MenuAction::Leave,
            },
            MenuItem {
                label: "Close buffer".to_string(),
                action: MenuAction::CloseBuffer,
            },
        ]
    }

    #[test]
    fn menu_open_populates_items_and_target() {
        let mut menu = ContextMenu::default();
        assert!(!menu.open);
        let id = BufferId::new(backend(), "#a");
        menu.open_at(4, 9, MenuTarget::Buffer(id.clone()), menu_items());

        assert!(menu.open);
        assert_eq!(menu.x, 4);
        assert_eq!(menu.y, 9);
        assert_eq!(menu.selected, 0);
        assert_eq!(menu.items.len(), 3);
        assert!(matches!(menu.target, Some(MenuTarget::Buffer(_))));
        assert_eq!(menu.selected_action(), Some(MenuAction::MarkRead));
    }

    #[test]
    fn menu_close_drops_items_and_target() {
        let mut menu = ContextMenu::default();
        menu.open_at(
            0,
            0,
            MenuTarget::Buffer(BufferId::status(backend())),
            menu_items(),
        );
        menu.close();
        assert!(!menu.open);
        assert!(menu.items.is_empty());
        assert!(menu.target.is_none());
        assert_eq!(menu.selected, 0);
        assert_eq!(menu.selected_action(), None);
    }

    #[test]
    fn menu_move_clamps_without_wrapping() {
        let mut menu = ContextMenu::default();
        menu.open_at(
            0,
            0,
            MenuTarget::Buffer(BufferId::status(backend())),
            menu_items(),
        );

        // Up at the top stays put.
        menu.move_up();
        assert_eq!(menu.selected, 0);

        menu.move_down();
        assert_eq!(menu.selected, 1);
        assert_eq!(menu.selected_action(), Some(MenuAction::Leave));

        menu.move_down();
        assert_eq!(menu.selected, 2);
        // Down at the last item stays put (no wrap to 0).
        menu.move_down();
        assert_eq!(menu.selected, 2);
        assert_eq!(menu.selected_action(), Some(MenuAction::CloseBuffer));
    }

    #[test]
    fn menu_resolved_rect_pushes_a_bottom_anchored_menu_fully_on_screen() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let mut menu = ContextMenu::default();
        // Anchored on the very last row, as a click on the bottom buffer bar
        // would be: the menu must slide up so its whole height fits.
        menu.open_at(
            2,
            23,
            MenuTarget::Buffer(BufferId::status(backend())),
            menu_items(),
        );

        let rect = menu.resolved_rect(area);
        assert!(
            rect.y + rect.height <= area.height,
            "menu bottom {} must fit within {}",
            rect.y + rect.height,
            area.height
        );
        assert!(
            rect.x + rect.width <= area.width,
            "menu right edge must fit on screen"
        );
        // 3 items + 2 borders.
        assert_eq!(rect.height, 5);
    }

    #[test]
    fn menu_item_at_maps_clicks_below_the_top_border() {
        let mut menu = ContextMenu::default();
        menu.open_at(
            0,
            0,
            MenuTarget::Buffer(BufferId::status(backend())),
            menu_items(),
        );
        // Pin a known rect (the renderer would write this).
        menu.rect = Rect {
            x: 0,
            y: 0,
            width: 14,
            height: 5,
        };

        assert_eq!(menu.item_at(2, 0), None, "top border is not an item");
        assert_eq!(menu.item_at(2, 1), Some(0), "first item below the border");
        assert_eq!(menu.item_at(2, 3), Some(2), "last item");
        assert_eq!(menu.item_at(2, 4), None, "bottom border row past the items");
        assert_eq!(menu.item_at(20, 1), None, "outside the menu");
    }

    #[test]
    fn selection_rows_are_ordered_regardless_of_drag_direction() {
        // Downward drag: anchor above cursor.
        let down = Selection {
            anchor: (3, 5),
            cursor: (10, 9),
        };
        assert_eq!(down.selected_rows(), 5..=9);

        // Upward drag: anchor below cursor. The range must still be low..=high.
        let up = Selection {
            anchor: (10, 9),
            cursor: (3, 5),
        };
        assert_eq!(up.selected_rows(), 5..=9);

        // A zero-length selection covers exactly its row.
        let point = Selection::new(4, 7);
        assert_eq!(point.selected_rows(), 7..=7);
    }

    #[test]
    fn toggle_copy_mode_flips_and_drops_selection_on_enter() {
        let mut view = ViewState::new();
        assert!(!view.copy_mode);

        view.selection = Some(Selection::new(1, 1));
        // Entering copy mode hands selection to the terminal, so the app-level
        // selection is cleared.
        assert!(view.toggle_copy_mode());
        assert!(view.copy_mode);
        assert!(view.selection.is_none());

        // Leaving copy mode flips back without re-creating a selection.
        assert!(!view.toggle_copy_mode());
        assert!(!view.copy_mode);
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

    #[test]
    fn edit_updates_body_and_marks_edited() {
        let mut state = test_state();

        state.apply(
            backend(),
            ChatEvent::Message {
                target: TargetId::from("#tirc"),
                id: Some(crate::core::EventId("$1".to_string())),
                sender: UserRef::new("alice"),
                body: MessageBody::plain("original"),
                kind: MsgKind::Text,
                echo_of: None,
                time: None,
            },
        );

        state.apply(
            backend(),
            ChatEvent::Edit {
                target: TargetId::from("#tirc"),
                id: crate::core::EventId("$1".to_string()),
                body: MessageBody::plain("updated"),
            },
        );

        let buf = buffer(&state, "#tirc");
        assert_eq!(buf.messages.len(), 1, "edit does not add a new message");
        assert!(buf.messages[0].edited, "edited flag is set");
        match &buf.messages[0].event {
            ChatEvent::Message { body, .. } => {
                assert_eq!(body.text, "updated", "body was replaced in place");
            }
            other => panic!("unexpected event type: {other:?}"),
        }
    }

    #[test]
    fn redaction_marks_message_as_redacted() {
        let mut state = test_state();

        state.apply(
            backend(),
            ChatEvent::Message {
                target: TargetId::from("#tirc"),
                id: Some(crate::core::EventId("$1".to_string())),
                sender: UserRef::new("alice"),
                body: MessageBody::plain("hi"),
                kind: MsgKind::Text,
                echo_of: None,
                time: None,
            },
        );

        state.apply(
            backend(),
            ChatEvent::Redaction {
                target: TargetId::from("#tirc"),
                id: crate::core::EventId("$1".to_string()),
                by: None,
            },
        );

        let buf = buffer(&state, "#tirc");
        assert_eq!(
            buf.messages.len(),
            1,
            "redaction does not add a new message"
        );
        assert!(buf.messages[0].redacted, "message is marked redacted");
    }

    #[test]
    fn reaction_increments_count_for_target_message() {
        let mut state = test_state();

        state.apply(
            backend(),
            ChatEvent::Message {
                target: TargetId::from("#tirc"),
                id: Some(crate::core::EventId("$1".to_string())),
                sender: UserRef::new("alice"),
                body: MessageBody::plain("hi"),
                kind: MsgKind::Text,
                echo_of: None,
                time: None,
            },
        );

        state.apply(
            backend(),
            ChatEvent::Reaction {
                target: TargetId::from("#tirc"),
                id: crate::core::EventId("$1".to_string()),
                sender: UserRef::new("bob"),
                key: "👍".to_string(),
                add: true,
            },
        );

        state.apply(
            backend(),
            ChatEvent::Reaction {
                target: TargetId::from("#tirc"),
                id: crate::core::EventId("$1".to_string()),
                sender: UserRef::new("carol"),
                key: "👍".to_string(),
                add: true,
            },
        );

        let buf = buffer(&state, "#tirc");
        assert_eq!(buf.messages.len(), 1, "reactions do not add new messages");
        assert_eq!(buf.messages[0].reactions.get("👍"), Some(&2));
    }
}
