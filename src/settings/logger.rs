use std::{
    fmt::{self, Write as _},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use tracing::{
    Event, Level, Metadata, Subscriber,
    field::{Field, Visit},
    span,
};

/// Minimal tracing subscriber: prints every event at or above `max_level`
/// as `HH:MM:SS LEVEL target: message key=value` on stderr.
struct Logger {
    max_level: Level,
    next_span_id: AtomicU64,
}

impl Subscriber for Logger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        // In tracing, more verbose levels compare as greater.
        *metadata.level() <= self.max_level
    }

    fn new_span(&self, _span: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(self.next_span_id.fetch_add(1, Ordering::Relaxed))
    }

    fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut line = LineVisitor(String::new());
        event.record(&mut line);
        let metadata = event.metadata();
        eprintln!(
            "{} {:>5} {}:{}",
            utc_time(),
            metadata.level(),
            metadata.target(),
            line.0
        );
    }

    fn enter(&self, _span: &span::Id) {}

    fn exit(&self, _span: &span::Id) {}
}

struct LineVisitor(String);

impl Visit for LineVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            let _ = write!(self.0, " {value}");
        } else {
            let _ = write!(self.0, " {}={value}", field.name());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.0, " {value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
}

fn utc_time() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
        % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds / 60) % 60,
        seconds % 60
    )
}

/// Installs the logger globally. `level` is one of trace, debug, info, warn, error.
pub fn init(level: &str) {
    let max_level = level.parse::<Level>().unwrap_or(Level::INFO);
    let logger = Logger {
        max_level,
        // Span ids must be non-zero.
        next_span_id: AtomicU64::new(1),
    };
    if tracing::subscriber::set_global_default(logger).is_err() {
        eprintln!("a global tracing subscriber was already installed");
    }
}
