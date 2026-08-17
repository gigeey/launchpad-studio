use std::collections::VecDeque;
use std::sync::Mutex;

use tracing::Subscriber;
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

const MAX_LOG_ENTRIES: usize = 2000;

static LOG_BUFFER: Mutex<Option<VecDeque<LogEntry>>> = Mutex::new(None);

#[derive(Clone, serde::Serialize)]
pub struct LogEntry {
    pub timestamp: String,
    pub level: String,
    pub target: String,
    pub message: String,
}

/// Initialize the global log buffer. Call once at startup before tracing init.
pub fn init() {
    let mut buf = LOG_BUFFER.lock().unwrap();
    *buf = Some(VecDeque::with_capacity(MAX_LOG_ENTRIES));
}

/// Get a snapshot of the current log entries.
pub fn get_logs(limit: usize) -> Vec<LogEntry> {
    let buf = LOG_BUFFER.lock().unwrap();
    match buf.as_ref() {
        Some(deque) => {
            let start = if deque.len() > limit {
                deque.len() - limit
            } else {
                0
            };
            deque.iter().skip(start).cloned().collect()
        }
        None => Vec::new(),
    }
}

/// Clear all buffered logs.
pub fn clear_logs() {
    let mut buf = LOG_BUFFER.lock().unwrap();
    if let Some(deque) = buf.as_mut() {
        deque.clear();
    }
}

fn push_entry(entry: LogEntry) {
    let mut buf = LOG_BUFFER.lock().unwrap();
    if let Some(deque) = buf.as_mut() {
        if deque.len() >= MAX_LOG_ENTRIES {
            deque.pop_front();
        }
        deque.push_back(entry);
    }
}

/// A tracing Layer that captures log events into the global ring buffer.
pub struct BufferLayer;

impl<S: Subscriber> Layer<S> for BufferLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();

        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);

        let entry = LogEntry {
            timestamp: chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
            level: meta.level().to_string(),
            target: meta.target().to_string(),
            message: visitor.0,
        };

        push_entry(entry);
    }
}

struct MessageVisitor(String);

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{:?}", value);
        } else if !self.0.is_empty() {
            self.0.push_str(&format!(" {}={:?}", field.name(), value));
        } else {
            self.0 = format!("{}={:?}", field.name(), value);
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0 = value.to_string();
        } else if !self.0.is_empty() {
            self.0.push_str(&format!(" {}={}", field.name(), value));
        } else {
            self.0 = format!("{}={}", field.name(), value);
        }
    }
}
