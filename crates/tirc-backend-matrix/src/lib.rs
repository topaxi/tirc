// matrix-sdk's e2e-encryption code has deeply nested async fns; without a raised
// limit the compiler overflows while proving the sync loop future is `Send`.
#![recursion_limit = "256"]

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
//! shared `Verifications` handle because the request arrives on a sync handler
//! while the user's accept/confirm commands arrive on the command loop.
//!
//! The sync driver translates Matrix timeline/state events into normalized
//! `ChatEvent`s and applies outgoing `Command`s to the client. The classic
//! `/sync` driver lives in `classic`; shared translation in `convert`,
//! authentication in `auth`, and SAS verification in `verify`.

mod auth;
mod classic;
mod convert;
mod sliding;
mod verify;

use tirc_core::backend::{BackendInfo, ChatBackend, CommandReceiver, EventSender};
use tirc_core::{BackendEvent, BackendId, BackendMessage, Protocol};

use auth::authenticate;
use convert::{emit, report_crypto_status, status_line};

/// Which sync driver to use. `Auto` (the default) probes the homeserver for
/// Simplified Sliding Sync (MSC4186) support after login; `On` and `Off` force
/// one driver, useful for testing and for servers that misreport support.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SlidingSyncMode {
    #[default]
    Auto,
    On,
    Off,
}

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
    /// Sync driver selection; see [`SlidingSyncMode`].
    pub sliding_sync: SlidingSyncMode,
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
        commands: CommandReceiver,
    ) -> anyhow::Result<()> {
        let id = self.id;
        let store_path = self.store_path()?;
        let session_path = Self::session_path(&store_path);
        // Cache directory for downloaded media (images shown inline). Best-effort:
        // a failure here only means images are not fetched, not that the backend
        // stops.
        let media_dir = store_path.join("media");
        let _ = std::fs::create_dir_all(&media_dir);

        emit(
            &events,
            id,
            status_line(format!("Connecting to {}...", self.config.homeserver)),
        );
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

        let use_sliding = match self.config.sliding_sync {
            SlidingSyncMode::On => true,
            SlidingSyncMode::Off => false,
            SlidingSyncMode::Auto => auth::supports_simplified_sliding_sync(&client).await,
        };

        if use_sliding {
            log::info!("using the simplified sliding sync driver");
            match sliding::run_sliding(
                client.clone(),
                id,
                &self.config,
                events.clone(),
                commands,
                &store_path,
                media_dir.clone(),
            )
            .await?
            {
                sliding::SlidingOutcome::Completed => Ok(()),
                // The homeserver advertised MSC4186 but the sync failed (e.g. an
                // SDK/server incompatibility). Fall back to the classic driver so
                // the connection still works, and say so in the status buffer.
                sliding::SlidingOutcome::FallBack(commands) => {
                    emit(
                        &events,
                        id,
                        status_line(
                            "Simplified sliding sync is unavailable on this homeserver; \
                             using classic sync instead."
                                .to_string(),
                        ),
                    );
                    classic::run_classic(
                        client,
                        id,
                        &self.config,
                        events,
                        commands,
                        &store_path,
                        media_dir,
                    )
                    .await
                }
            }
        } else {
            classic::run_classic(
                client,
                id,
                &self.config,
                events,
                commands,
                &store_path,
                media_dir,
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::join;
    use matrix_sdk::config::SyncSettings;
    use matrix_sdk::ruma::RoomId;
    use matrix_sdk::Client;
    use std::time::Duration;
    use tirc_core::{ChatEvent, Command, MsgKind, TargetId, TxnId};
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
            sliding_sync: SlidingSyncMode::default(),
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
            sliding_sync: SlidingSyncMode::default(),
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
            sliding_sync: SlidingSyncMode::default(),
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
