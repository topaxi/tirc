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
    /// Extra CA certificate (PEM) to trust for this homeserver, on top of the
    /// system trust store.
    pub root_ca_pem: Option<String>,
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
    ///
    /// Keyed by user id *and* homeserver, not user id alone: two distinct
    /// homeservers can otherwise share an MXID (e.g. two local test servers
    /// both named `localhost`, as tirc's own dev/matrix setup does), and
    /// reusing one store for both corrupts the crypto state - the SDK errors
    /// with "the account in the store doesn't match the account in the
    /// constructor" as soon as the second backend opens the shared store.
    fn store_path(&self) -> anyhow::Result<std::path::PathBuf> {
        if let Some(dir) = &self.config.store_dir {
            std::fs::create_dir_all(dir)?;
            return Ok(dir.clone());
        }

        let sanitize = |s: &str| -> String {
            s.chars()
                .map(|c| if c.is_alphanumeric() { c } else { '_' })
                .collect()
        };
        let user = sanitize(&self.config.user_id);
        let homeserver = sanitize(&self.config.homeserver);

        Ok(xdg::BaseDirectories::with_prefix("tirc")
            .create_data_directory(format!("matrix/{user}@{homeserver}"))?)
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
        let home_server = client.user_id().map(|user| user.server_name().to_string());
        let _ = events.send(BackendMessage {
            backend: id,
            event: BackendEvent::Ready {
                nickname: nickname.clone(),
                home_server,
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
    /// TIRC_TEST_HOMESERVER=https://localhost:8448 \
    /// TIRC_TEST_USER=@alice:dendrite.local TIRC_TEST_PASSWORD=alicepassword \
    /// TIRC_TEST_ROOM='!roomid:dendrite.local' \
    /// TIRC_TEST_ROOT_CA_PEM=dev/matrix/tls/ca.cert.pem \
    ///   cargo test --lib matrix::tests -- --ignored --nocapture
    /// ```
    ///
    /// `TIRC_TEST_ROOM` must be the exact room id returned by `createRoom` -
    /// some room versions use server-less ids with no `:server` suffix, and an
    /// over-qualified id will not resolve.
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
            root_ca_pem: std::env::var("TIRC_TEST_ROOT_CA_PEM")
                .ok()
                .map(|path| std::fs::read_to_string(path).unwrap()),
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

    /// Regression test for a sliding-sync-only bug: the Timeline's own
    /// `EventTimelineItem::transaction_id()` is only populated while the SDK
    /// still considers an item a local echo, and can already be gone by the
    /// first diff our subscriber sees for the confirmed event - especially
    /// under a federated room's extra latency. Without `PendingEchoes` as a
    /// fallback, the confirmed message's `echo_of` comes back `None`,
    /// `apply_message` never matches it to the pending optimistic echo, and
    /// the line duplicates (one stuck pending forever, one confirmed) instead
    /// of replacing in place. Run against a `sliding_sync = 'on'` homeserver
    /// (see the module doc for env vars); irrelevant to the classic driver.
    #[tokio::test]
    #[ignore = "requires the local matrix homeserver from dev/matrix"]
    async fn sliding_send_confirms_with_matching_echo_of() {
        let config = MatrixBackendConfig {
            homeserver: std::env::var("TIRC_TEST_HOMESERVER").unwrap(),
            user_id: std::env::var("TIRC_TEST_USER").unwrap(),
            password: std::env::var("TIRC_TEST_PASSWORD").unwrap(),
            device_id: None,
            autojoin: vec![std::env::var("TIRC_TEST_ROOM").unwrap()],
            store_dir: Some(unique_store_dir()),
            sliding_sync: SlidingSyncMode::On,
            root_ca_pem: std::env::var("TIRC_TEST_ROOT_CA_PEM")
                .ok()
                .map(|path| std::fs::read_to_string(path).unwrap()),
        };
        let room = config.autojoin[0].clone();
        let marker = format!("tirc-test-{}", std::process::id());

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
                body: marker.clone(),
                kind: MsgKind::Text,
                txn: TxnId(1),
            })
            .unwrap();

        // The pending optimistic echo (id: None) always arrives first; wait
        // specifically for the *confirmed* delivery (a real id) and assert it
        // carries the matching echo_of, so `apply_message` replaces the
        // pending line in place instead of leaving both around.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let mut confirmed_echo_of = None;
        while let Ok(Some(m)) = tokio::time::timeout_at(deadline, event_rx.recv()).await {
            if let BackendEvent::Event(ChatEvent::Message {
                id: Some(_),
                echo_of,
                body,
                ..
            }) = &m.event
            {
                if body.text == marker {
                    confirmed_echo_of = Some(*echo_of);
                    break;
                }
            }
        }

        assert_eq!(
            confirmed_echo_of,
            Some(Some(TxnId(1))),
            "expected the confirmed delivery to carry echo_of matching the sent txn"
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
            root_ca_pem: std::env::var("TIRC_TEST_ROOT_CA_PEM")
                .ok()
                .map(|path| std::fs::read_to_string(path).unwrap()),
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
            root_ca_pem: std::env::var("TIRC_TEST_ROOT_CA_PEM")
                .ok()
                .map(|path| std::fs::read_to_string(path).unwrap()),
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
