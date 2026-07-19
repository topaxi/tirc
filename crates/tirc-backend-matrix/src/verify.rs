//! Interactive (SAS) device verification, shared by both sync drivers: the
//! incoming-request handler, the `:verify` command dispatch, and the background
//! tasks driving a request through the emoji exchange.

use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::Mutex;

use matrix_sdk::encryption::verification::{
    SasState, SasVerification, Verification, VerificationRequest, VerificationRequestState,
};
use matrix_sdk::ruma::events::key::verification::request::ToDeviceKeyVerificationRequestEvent;
use matrix_sdk::ruma::UserId;
use matrix_sdk::Client;

use tirc_core::backend::EventSender;
use tirc_core::{BackendId, VerifyAction};

use crate::convert::{emit, status_line};

/// Shared in-flight verification state. The incoming-request handler and the
/// command loop run on different tasks, so the pending request and the active
/// SAS flow are held behind a mutex both can reach. Both fields are cheap SDK
/// handles, so cloning out of the lock to act on them is fine.
#[derive(Clone, Default)]
pub(crate) struct Verifications {
    /// An incoming request awaiting `:verify accept`.
    pending: Arc<Mutex<Option<VerificationRequest>>>,
    /// The SAS flow currently presenting an emoji string, for `:verify confirm`
    /// / `:verify cancel`.
    sas: Arc<Mutex<Option<SasVerification>>>,
}

impl Verifications {
    async fn set_pending(&self, request: VerificationRequest) {
        *self.pending.lock().await = Some(request);
    }

    async fn take_pending(&self) -> Option<VerificationRequest> {
        self.pending.lock().await.take()
    }

    async fn set_sas(&self, sas: SasVerification) {
        *self.sas.lock().await = Some(sas);
    }

    async fn clear_sas(&self) {
        *self.sas.lock().await = None;
    }

    async fn current_sas(&self) -> Option<SasVerification> {
        self.sas.lock().await.clone()
    }
}

/// Registers the handler surfacing an incoming verification request. The request
/// is held pending so the user can vet it before it starts exchanging keys: we
/// surface who asked and how to proceed; the request is fetched by its
/// transaction id and stashed for `:verify accept` / `:verify cancel`.
pub(crate) fn register_verification_handler(
    client: &Client,
    id: BackendId,
    events: EventSender,
    verifications: Verifications,
) {
    client.add_event_handler(
        move |event: ToDeviceKeyVerificationRequestEvent, client: Client| {
            let events = events.clone();
            let verifications = verifications.clone();
            async move {
                let Some(request) = client
                    .encryption()
                    .get_verification_request(&event.sender, &event.content.transaction_id)
                    .await
                else {
                    return;
                };

                verifications.set_pending(request).await;
                emit(
                    &events,
                    id,
                    status_line(format!(
                        "Device verification requested by {} (device {}). \
                         Type :verify accept to compare the emoji, or :verify cancel to reject.",
                        event.sender, event.content.from_device
                    )),
                );
            }
        },
    );
}

/// Handles a `:verify ...` command. Accept/confirm/cancel act on the in-flight
/// verification; a bare `:verify [user]` initiates one.
pub(crate) async fn apply_verify(
    client: &Client,
    id: BackendId,
    events: &EventSender,
    verifications: &Verifications,
    action: VerifyAction,
) {
    match action {
        VerifyAction::Request { user } => {
            start_verification(client, id, events, verifications, user).await
        }
        VerifyAction::Accept => accept_verification(id, events, verifications).await,
        VerifyAction::Confirm => confirm_verification(id, events, verifications).await,
        VerifyAction::Cancel => cancel_verification(id, events, verifications).await,
    }
}

/// Initiates verification of `user` (or our own identity for self-verification
/// when `None`), then drives the resulting request to completion.
async fn start_verification(
    client: &Client,
    id: BackendId,
    events: &EventSender,
    verifications: &Verifications,
    user: Option<String>,
) {
    let user_id = match user {
        Some(user) => match UserId::parse(&user) {
            Ok(user_id) => user_id,
            Err(err) => {
                emit(
                    events,
                    id,
                    status_line(format!("Invalid user id {user:?}: {err}")),
                );
                return;
            }
        },
        None => match client.user_id() {
            Some(user_id) => user_id.to_owned(),
            None => return,
        },
    };

    let identity = match client.encryption().get_user_identity(&user_id).await {
        Ok(Some(identity)) => identity,
        Ok(None) => {
            emit(
                events,
                id,
                status_line(format!(
                    "Cannot verify {user_id}: no cross-signing identity (they have not set up \
                     verification)."
                )),
            );
            return;
        }
        Err(err) => {
            emit(
                events,
                id,
                status_line(format!("Verification lookup failed: {err}")),
            );
            return;
        }
    };

    match identity.request_verification().await {
        Ok(request) => {
            emit(
                events,
                id,
                status_line(format!(
                    "Verification request sent to {user_id}; waiting for them to accept."
                )),
            );
            spawn_drive_request(id, events.clone(), verifications.clone(), request);
        }
        Err(err) => emit(
            events,
            id,
            status_line(format!("Could not request verification: {err}")),
        ),
    }
}

/// Accepts the pending incoming request and drives it forward.
async fn accept_verification(id: BackendId, events: &EventSender, verifications: &Verifications) {
    let Some(request) = verifications.take_pending().await else {
        emit(
            events,
            id,
            status_line("No pending verification request to accept.".to_string()),
        );
        return;
    };

    if let Err(err) = request.accept().await {
        emit(
            events,
            id,
            status_line(format!("Failed to accept verification: {err}")),
        );
        return;
    }

    emit(
        events,
        id,
        status_line("Verification accepted; waiting for the emoji to compare...".to_string()),
    );
    spawn_drive_request(id, events.clone(), verifications.clone(), request);
}

/// Confirms the displayed short-auth-string matches, telling the SDK our device
/// trusts the other one.
async fn confirm_verification(id: BackendId, events: &EventSender, verifications: &Verifications) {
    let Some(sas) = verifications.current_sas().await else {
        emit(
            events,
            id,
            status_line("No verification in progress to confirm.".to_string()),
        );
        return;
    };

    match sas.confirm().await {
        Ok(()) => emit(
            events,
            id,
            status_line(
                "Marked the emoji as matching; waiting for the other device...".to_string(),
            ),
        ),
        Err(err) => emit(
            events,
            id,
            status_line(format!("Failed to confirm verification: {err}")),
        ),
    }
}

/// Cancels the in-flight SAS, or rejects a still-pending request when no SAS has
/// started yet.
async fn cancel_verification(id: BackendId, events: &EventSender, verifications: &Verifications) {
    if let Some(sas) = verifications.current_sas().await {
        let _ = sas.cancel().await;
        verifications.clear_sas().await;
        emit(
            events,
            id,
            status_line("Verification cancelled.".to_string()),
        );
        return;
    }

    if let Some(request) = verifications.take_pending().await {
        let _ = request.cancel().await;
        emit(
            events,
            id,
            status_line("Verification request rejected.".to_string()),
        );
        return;
    }

    emit(
        events,
        id,
        status_line("No verification to cancel.".to_string()),
    );
}

/// Spawns the background task that follows a verification request through its
/// state changes until it transitions into a SAS flow (then hands off to
/// [`drive_sas`]) or terminates.
fn spawn_drive_request(
    id: BackendId,
    events: EventSender,
    verifications: Verifications,
    request: VerificationRequest,
) {
    tokio::spawn(async move {
        drive_request(id, &events, &verifications, request).await;
    });
}

/// Follows a verification request's state stream. The side that initiated the
/// request starts the SAS once both are ready; the other side waits for the
/// resulting transition. Either way we end up driving the SAS emoji exchange.
async fn drive_request(
    id: BackendId,
    events: &EventSender,
    verifications: &Verifications,
    request: VerificationRequest,
) {
    let mut stream = request.changes();
    while let Some(state) = stream.next().await {
        match state {
            VerificationRequestState::Ready { .. } if request.we_started() => {
                match request.start_sas().await {
                    Ok(Some(sas)) => {
                        drive_sas(id, events, verifications, sas).await;
                        return;
                    }
                    Ok(None) => {}
                    Err(err) => {
                        emit(
                            events,
                            id,
                            status_line(format!("Could not start verification: {err}")),
                        );
                        return;
                    }
                }
            }
            VerificationRequestState::Transitioned {
                verification: Verification::SasV1(sas),
            } => {
                drive_sas(id, events, verifications, sas).await;
                return;
            }
            VerificationRequestState::Cancelled(info) => {
                emit(
                    events,
                    id,
                    status_line(format!("Verification cancelled: {}", info.reason())),
                );
                return;
            }
            VerificationRequestState::Done => return,
            _ => {}
        }
    }
}

/// Drives a SAS flow: accepts it (unless we started it), publishes the emoji
/// short-auth-string for the user to compare, and reports the outcome. The SAS
/// is stashed in [`Verifications`] so `:verify confirm` / `:verify cancel` can
/// act on it while this loop awaits the next state change.
async fn drive_sas(
    id: BackendId,
    events: &EventSender,
    verifications: &Verifications,
    sas: SasVerification,
) {
    if !sas.we_started() {
        if let Err(err) = sas.accept().await {
            emit(
                events,
                id,
                status_line(format!("Failed to start emoji verification: {err}")),
            );
            return;
        }
    }

    verifications.set_sas(sas.clone()).await;

    let mut stream = sas.changes();
    while let Some(state) = stream.next().await {
        match state {
            SasState::KeysExchanged { emojis, decimals } => {
                if let Some(emojis) = emojis {
                    emit(
                        events,
                        id,
                        status_line(format!("Compare emoji: {}", format_emojis(&emojis.emojis))),
                    );
                } else {
                    let (a, b, c) = decimals;
                    emit(
                        events,
                        id,
                        status_line(format!("Compare numbers: {a} {b} {c}")),
                    );
                }
                emit(
                    events,
                    id,
                    status_line(
                        "If they match the other device, run :verify confirm; otherwise :verify cancel."
                            .to_string(),
                    ),
                );
            }
            SasState::Done { .. } => {
                emit(
                    events,
                    id,
                    status_line("Device verified successfully.".to_string()),
                );
                break;
            }
            SasState::Cancelled(info) => {
                emit(
                    events,
                    id,
                    status_line(format!("Verification cancelled: {}", info.reason())),
                );
                break;
            }
            _ => {}
        }
    }

    verifications.clear_sas().await;
}

/// Formats the seven SAS emojis as `symbol description` pairs on one line.
fn format_emojis(emojis: &[matrix_sdk::encryption::verification::Emoji; 7]) -> String {
    emojis
        .iter()
        .map(|emoji| format!("{} {}", emoji.symbol, emoji.description))
        .collect::<Vec<_>>()
        .join("   ")
}
