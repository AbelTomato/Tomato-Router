//! In-memory tracing buffer used by the TUI instead of writing to stdout.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use time::{OffsetDateTime, UtcOffset};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

const MAX_MESSAGE_CHARS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }

    fn from_tracing(level: &tracing::Level) -> Self {
        match *level {
            tracing::Level::TRACE => Self::Trace,
            tracing::Level::DEBUG => Self::Debug,
            tracing::Level::INFO => Self::Info,
            tracing::Level::WARN => Self::Warn,
            tracing::Level::ERROR => Self::Error,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LogLine {
    pub time: String,
    pub level: LogLevel,
    pub target: String,
    pub message: String,
}

#[derive(Default)]
struct Inner {
    lines: VecDeque<LogLine>,
    capacity: usize,
    version: u64,
    dropped: u64,
}

#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<Inner>>,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                lines: VecDeque::with_capacity(capacity.min(1024)),
                capacity: capacity.max(1),
                version: 0,
                dropped: 0,
            })),
        }
    }

    pub fn push(&self, line: LogLine) {
        let mut inner = self.inner.lock().expect("log buffer mutex poisoned");
        while inner.lines.len() >= inner.capacity {
            inner.lines.pop_front();
            inner.dropped = inner.dropped.saturating_add(1);
        }
        inner.lines.push_back(line);
        inner.version = inner.version.wrapping_add(1);
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock().expect("log buffer mutex poisoned");
        inner.lines.clear();
        inner.dropped = 0;
        inner.version = inner.version.wrapping_add(1);
    }

    pub fn version(&self) -> u64 {
        self.inner
            .lock()
            .expect("log buffer mutex poisoned")
            .version
    }

    pub fn dropped(&self) -> u64 {
        self.inner
            .lock()
            .expect("log buffer mutex poisoned")
            .dropped
    }

    pub fn snapshot(&self) -> Vec<LogLine> {
        self.inner
            .lock()
            .expect("log buffer mutex poisoned")
            .lines
            .iter()
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("log buffer mutex poisoned")
            .lines
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

struct TuiLogLayer {
    buffer: LogBuffer,
    offset: UtcOffset,
}

impl TuiLogLayer {
    fn new(buffer: LogBuffer, offset: UtcOffset) -> Self {
        Self { buffer, offset }
    }
}

impl<S: Subscriber> Layer<S> for TuiLogLayer {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        let message = visitor.finish();
        if message.is_empty() {
            return;
        }
        let now = OffsetDateTime::now_utc().to_offset(self.offset);
        self.buffer.push(LogLine {
            time: format!("{:02}:{:02}:{:02}", now.hour(), now.minute(), now.second()),
            level: LogLevel::from_tracing(event.metadata().level()),
            target: event.metadata().target().to_string(),
            message,
        });
    }
}

#[derive(Default)]
struct EventVisitor {
    message: Option<String>,
    fields: Vec<String>,
}

impl EventVisitor {
    fn finish(self) -> String {
        let mut text = self.message.unwrap_or_default();
        for field in self.fields {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(&field);
        }
        if text.chars().count() > MAX_MESSAGE_CHARS {
            text = text.chars().take(MAX_MESSAGE_CHARS).collect();
            text.push_str(" ...");
        }
        text
    }
}

fn debug_text(value: &dyn std::fmt::Debug) -> String {
    let text = format!("{value:?}");
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        text[1..text.len() - 1].to_string()
    } else {
        text
    }
}

impl Visit for EventVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(debug_text(value));
        } else {
            self.fields
                .push(format!("{}={}", field.name(), debug_text(value)));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.fields.push(format!("{}={value}", field.name()));
        }
    }
}

/// Initialize a text-only tracing subscriber that writes into `buffer`.
pub fn init_tracing(filter: Option<&str>, buffer: LogBuffer) -> Result<()> {
    let filter = filter
        .map(str::to_string)
        .or_else(|| std::env::var("RUST_LOG").ok())
        .unwrap_or_else(|| "tomato_router=info,tower_http=warn".to_string());
    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(TuiLogLayer::new(buffer, offset))
        .try_init()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_drops_oldest_lines() {
        let buffer = LogBuffer::new(2);
        for index in 0..3 {
            buffer.push(LogLine {
                time: "00:00:00".to_string(),
                level: LogLevel::Info,
                target: "test".to_string(),
                message: index.to_string(),
            });
        }
        let lines = buffer.snapshot();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].message, "1");
        assert_eq!(lines[1].message, "2");
        assert_eq!(buffer.dropped(), 1);
    }
}
