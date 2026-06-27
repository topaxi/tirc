//! Matrix backend: the sole place that touches `matrix-sdk`.
//!
//! Unencrypted rooms only for now (E2E is a deferred follow-up). The SDK's sync
//! loop drives incoming events through registered handlers that translate Matrix
//! timeline/state events into normalized [`ChatEvent`]s; outgoing [`Command`]s
//! are applied directly to the client.

use matrix_sdk::config::SyncSettings;
use matrix_sdk::ruma::events::room::member::MembershipState;
use matrix_sdk::ruma::events::room::member::SyncRoomMemberEvent;
use matrix_sdk::ruma::events::room::message::{
    MessageType, RoomMessageEventContent, SyncRoomMessageEvent,
};
use matrix_sdk::ruma::events::room::topic::SyncRoomTopicEvent;
use matrix_sdk::ruma::{OwnedRoomId, RoomId, UserId};
use matrix_sdk::{Client, Room};

use crate::core::{
    BackendEvent, BackendId, BackendMessage, ChatEvent, Command, EventId, Formatted,
    MembershipChange, MessageBody, MsgKind, Protocol, TargetId, UserRef,
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
        let sanitized: String = self
            .config
            .user_id
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();

        Ok(xdg::BaseDirectories::with_prefix("tirc")
            .create_data_directory(format!("matrix/{sanitized}"))?)
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

        let client = Client::builder()
            .homeserver_url(&self.config.homeserver)
            .sqlite_store(&store_path, None)
            .build()
            .await?;

        let mut login = client
            .matrix_auth()
            .login_username(&self.config.user_id, &self.config.password)
            .initial_device_display_name("tirc");
        if let Some(device_id) = &self.config.device_id {
            login = login.device_id(device_id);
        }
        login.await?;

        let nickname = client
            .user_id()
            .map(|user| user.localpart().to_string())
            .unwrap_or_else(|| self.config.user_id.clone());
        let _ = events.send(BackendMessage {
            backend: id,
            event: BackendEvent::Ready { nickname },
        });

        register_handlers(&client, id, events.clone());

        // Autojoin configured rooms (aliases or ids).
        for room in &self.config.autojoin {
            let _ = join(&client, room).await;
        }

        // Drive the SDK sync loop in the background; handle commands here until
        // the backend's command channel closes.
        let sync_client = client.clone();
        let sync = tokio::spawn(async move {
            let _ = sync_client.sync(SyncSettings::default()).await;
        });

        while let Some(command) = commands.recv().await {
            apply_command(&client, command).await;
        }

        sync.abort();
        Ok(())
    }
}

/// Registers sync handlers translating Matrix events into [`ChatEvent`]s.
fn register_handlers(client: &Client, id: BackendId, events: EventSender) {
    let message_events = events.clone();
    client.add_event_handler(move |event: SyncRoomMessageEvent, room: Room| {
        let events = message_events.clone();
        async move {
            if let SyncRoomMessageEvent::Original(event) = event {
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
                    _ => return,
                };

                emit(
                    &events,
                    id,
                    ChatEvent::Message {
                        target: room_target(&room),
                        id: Some(EventId(event.event_id.to_string())),
                        sender: sender_ref(&room, &event.sender).await,
                        body,
                        kind,
                        echo_of: None,
                    },
                );
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
                    display: event.content.displayname.clone(),
                };

                emit(
                    &events,
                    id,
                    ChatEvent::Membership {
                        target: room_target(&room),
                        who,
                        change,
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
                    },
                );
            }
        }
    });
}

/// Applies an outgoing command to the Matrix client. Matrix has native local
/// echo via the sync timeline, so sends do not emit an optimistic copy.
async fn apply_command(client: &Client, command: Command) {
    match command {
        Command::SendMessage {
            target, body, kind, ..
        } => {
            let Some(room) = room_by_target(client, &target) else {
                return;
            };
            let content = match kind {
                MsgKind::Action => RoomMessageEventContent::emote_plain(body),
                MsgKind::Notice => RoomMessageEventContent::notice_plain(body),
                _ => RoomMessageEventContent::text_plain(body),
            };
            let _ = room.send(content).await;
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
        // Reactions/redactions and IRC-only commands are not handled yet.
        _ => {}
    }
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

/// Resolves a sender into a [`UserRef`], using the room-local display name when
/// available (no network round-trip).
async fn sender_ref(room: &Room, user: &UserId) -> UserRef {
    let display = room
        .get_member_no_sync(user)
        .await
        .ok()
        .flatten()
        .and_then(|member| member.display_name().map(str::to_string));

    UserRef {
        id: user.to_string(),
        display,
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
    /// TIRC_TEST_ROOM='!...:localhost' \
    ///   cargo test --lib matrix::tests -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "requires the local matrix homeserver from dev/matrix"]
    async fn login_join_send_roundtrip() {
        let config = MatrixBackendConfig {
            homeserver: std::env::var("TIRC_TEST_HOMESERVER").unwrap(),
            user_id: std::env::var("TIRC_TEST_USER").unwrap(),
            password: std::env::var("TIRC_TEST_PASSWORD").unwrap(),
            device_id: None,
            autojoin: vec![std::env::var("TIRC_TEST_ROOM").unwrap()],
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
