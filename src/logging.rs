//! In-memory log capture for the `:debug` pane.
//!
//! No logger was previously installed, so `log::*` calls (and third-party
//! `tracing` events) went nowhere. Here we install a [`tracing`] subscriber with
//! two parts: the [`tracing_log`] bridge routes the `log` facade (our own
//! macros) into `tracing`, and a custom [`MemoryLayer`] appends every event into
//! a bounded ring buffer the renderer reads to draw the debug pane. A file or
//! `fmt` sink can be layered on later without touching the capture path.

use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};

use chrono::{DateTime, Local};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::{registry::LookupSpan, util::SubscriberInitExt, EnvFilter};

/// Upper bound on retained log lines; older lines are dropped. Bounds memory so a
/// long-running session with a chatty filter cannot grow without limit.
const MAX_LINES: usize = 2000;

/// One captured log record, as shown in the debug pane.
#[derive(Clone, Debug)]
pub struct LogLine {
    pub time: DateTime<Local>,
    pub level: Level,
    pub target: String,
    pub message: String,
}

static LOG_BUFFER: LazyLock<Mutex<VecDeque<LogLine>>> =
    LazyLock::new(|| Mutex::new(VecDeque::with_capacity(MAX_LINES)));

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

/// A clone of the most recent `max` captured lines, oldest first. Read on the UI
/// thread; writers are backend threads, hence the `Mutex`.
pub fn recent(max: usize) -> Vec<LogLine> {
    let buffer = LOG_BUFFER.lock().unwrap();
    let start = buffer.len().saturating_sub(max);
    buffer.iter().skip(start).cloned().collect()
}

fn push(line: LogLine) {
    let mut buffer = LOG_BUFFER.lock().unwrap();
    if buffer.len() == MAX_LINES {
        buffer.pop_front();
    }
    buffer.push_back(line);
}

/// A `tracing` layer that appends every event to the ring buffer.
struct MemoryLayer;

impl<S> Layer<S> for MemoryLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);

        let metadata = event.metadata();
        push(LogLine {
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
    fn captures_log_events_into_the_ring_buffer() {
        init();
        let marker = "tirc-debug-pane-test-marker";
        log::warn!("{marker}");

        let found = recent(MAX_LINES)
            .into_iter()
            .find(|line| line.message.contains(marker));
        let line = found.expect("logged message should be captured");
        assert_eq!(line.level, Level::WARN);
    }
}
