//! In-memory log capture for the debug log buffer.
//!
//! No logger was previously installed, so `log::*` calls (and third-party
//! `tracing` events) went nowhere. Here we install a [`tracing`] subscriber with
//! two parts: the [`tracing_log`] bridge routes the `log` facade (our own
//! macros) into `tracing`, and a custom [`MemoryLayer`] forwards every event onto
//! an unbounded channel. The main loop drains the channel and appends each line
//! to the synthetic debug backend's buffer, so logs render like any other buffer.
//! A file or `fmt` sink can be layered on later without touching the capture path.

use std::sync::{LazyLock, Mutex};

use chrono::{DateTime, Local, Utc};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};

/// Re-exported so downstream crates can name a [`LogLine`]'s level type without
/// depending on `tracing` directly.
pub use tracing::Level;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::{registry::LookupSpan, util::SubscriberInitExt, EnvFilter};

use crate::ChatEvent;

/// One captured log record, delivered to the debug buffer.
#[derive(Clone, Debug)]
pub struct LogLine {
    pub time: DateTime<Local>,
    pub level: Level,
    pub target: String,
    pub message: String,
}

impl LogLine {
    /// Renders this record as a status-buffer line for the debug buffer. The log
    /// level rides in `code` (so themes can color by severity) and the log target
    /// in `from`; the message time is preserved so lines sort in order.
    pub fn into_event(self) -> ChatEvent {
        ChatEvent::ServerInfo {
            target: None,
            from: Some(self.target),
            code: Some(self.level.as_str().to_string()),
            text: self.message,
            raw: None,
            time: Some(self.time.with_timezone(&Utc)),
        }
    }
}

/// The eagerly-created log channel: a sender fed by every logging thread and a
/// receiver taken once by the main loop.
type LogChannel = (
    UnboundedSender<LogLine>,
    Mutex<Option<UnboundedReceiver<LogLine>>>,
);

/// Sender half feeds captured lines from arbitrary logging threads; the receiver
/// half is claimed once by the main loop via [`take_receiver`]. Created eagerly so
/// lines logged before the runtime exists (e.g. config loading) queue here and are
/// drained once the loop starts.
static LOG_CHANNEL: LazyLock<LogChannel> = LazyLock::new(|| {
    let (tx, rx) = unbounded_channel();
    (tx, Mutex::new(Some(rx)))
});

/// Installs the global logging subscriber. Idempotent-ish: a second call (or a
/// pre-existing subscriber) is reported and ignored rather than panicking, so
/// tests and re-entrant setups stay safe.
pub fn init() {
    // Route the `log` facade into `tracing` so our `log::*` macros are captured
    // alongside native `tracing` events from matrix-sdk/reqwest/tungstenite.
    if let Err(err) = tracing_log::LogTracer::init() {
        log::warn!("log-to-tracing bridge already installed: {err}");
    }

    // Keep third-party crates at info by default (no flood) while our own crate is
    // verbose; `RUST_LOG` overrides the whole thing.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,tirc=debug"));

    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(MemoryLayer);
    if let Err(err) = subscriber.try_init() {
        log::warn!("tracing subscriber already installed: {err}");
    }
}

/// Claims the receiver end of the log channel. Returns `Some` on the first call
/// and `None` afterwards; the main loop takes it exactly once.
pub fn take_receiver() -> Option<UnboundedReceiver<LogLine>> {
    LOG_CHANNEL.1.lock().unwrap().take()
}

/// A `tracing` layer that forwards every event onto the log channel.
struct MemoryLayer;

impl<S> Layer<S> for MemoryLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);

        let metadata = event.metadata();
        // A closed receiver (main loop gone) just drops the line.
        let _ = LOG_CHANNEL.0.send(LogLine {
            time: Local::now(),
            level: *metadata.level(),
            target: metadata.target().to_string(),
            message: visitor.finish(),
        });
    }
}

/// Collects an event's `message` field plus any other fields into a single line,
/// matching the familiar `message key=value` rendering.
#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl MessageVisitor {
    fn finish(self) -> String {
        if self.fields.is_empty() {
            self.message
        } else if self.message.is_empty() {
            self.fields.trim_start().to_string()
        } else {
            format!("{}{}", self.message, self.fields)
        }
    }
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.fields
                .push_str(&format!(" {}={:?}", field.name(), value));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields
                .push_str(&format!(" {}={}", field.name(), value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwards_log_events_onto_the_channel() {
        init();
        let mut rx = take_receiver().expect("receiver available on first take");
        let marker = "tirc-debug-buffer-test-marker";
        log::warn!("{marker}");

        // Drain until our marker shows up (other events may precede it).
        loop {
            let line = rx.try_recv().expect("logged message should be captured");
            if line.message.contains(marker) {
                assert_eq!(line.level, Level::WARN);
                break;
            }
        }
    }
}
