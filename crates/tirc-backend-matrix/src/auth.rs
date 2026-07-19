//! Client construction and authentication: builds the sqlite-backed client and
//! restores a persisted login session (or falls back to a password login),
//! keeping the device stable across runs.

use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::Client;

use crate::MatrixBackendConfig;

/// Builds and authenticates the client, reusing a persisted session when
/// possible so we do not register a new device on every connect. A restored
/// session is validated with a `whoami` round-trip; if the device was signed out
/// remotely the stale session is discarded and we fall back to a password login.
///
/// A fresh client is built for the fallback because `restore_session` and
/// `login` both panic if a session was already set on the same client.
pub(crate) async fn authenticate(
    config: &MatrixBackendConfig,
    store_path: &std::path::Path,
    session_path: &std::path::Path,
) -> anyhow::Result<Client> {
    let build_client = || async {
        Client::builder()
            .homeserver_url(&config.homeserver)
            .sqlite_store(store_path, None)
            .build()
            .await
    };

    if let Some(session) = load_session(session_path) {
        let client = build_client().await?;
        client.restore_session(session).await?;
        match client.whoami().await {
            Ok(_) => return Ok(client),
            Err(err) => {
                log::warn!("persisted matrix session is no longer valid ({err}); logging in again");
                let _ = std::fs::remove_file(session_path);
            }
        }
    }

    let client = build_client().await?;
    let mut login = client
        .matrix_auth()
        .login_username(&config.user_id, &config.password)
        .initial_device_display_name("tirc");
    if let Some(device_id) = &config.device_id {
        login = login.device_id(device_id);
    }
    login.await?;

    if let Some(session) = client.matrix_auth().session() {
        save_session(session_path, &session);
    }

    Ok(client)
}

/// Reads a previously persisted [`MatrixSession`], returning `None` when there is
/// no saved session or it cannot be parsed (in which case we fall back to a fresh
/// password login).
fn load_session(path: &std::path::Path) -> Option<MatrixSession> {
    let data = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&data) {
        Ok(session) => Some(session),
        Err(err) => {
            log::warn!("ignoring unreadable matrix session at {path:?}: {err}");
            None
        }
    }
}

/// Persists the login session so the next run restores it instead of logging in
/// again. Best-effort: a write failure only means we log in afresh next time.
fn save_session(path: &std::path::Path, session: &MatrixSession) {
    match serde_json::to_string(session) {
        Ok(data) => {
            if let Err(err) = std::fs::write(path, data) {
                log::warn!("failed to persist matrix session to {path:?}: {err}");
            }
        }
        Err(err) => log::warn!("failed to serialize matrix session: {err}"),
    }
}
