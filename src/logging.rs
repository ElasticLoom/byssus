//! Structured `key=value` (logfmt) logging to stderr.
//!
//! See `docs/DESIGN.md`, "Logging".

use std::fmt::{self, Write as _};
use std::os::fd::AsFd;
use std::str::FromStr;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

/// A log level accepted on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogLevel(pub Level);

impl FromStr for LogLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "error" => Ok(Self(Level::ERROR)),
            "warn" => Ok(Self(Level::WARN)),
            "info" => Ok(Self(Level::INFO)),
            "debug" => Ok(Self(Level::DEBUG)),
            "trace" => Ok(Self(Level::TRACE)),
            _ => Err(format!(
                "invalid log level '{s}' (expected error, warn, info, debug or trace)"
            )),
        }
    }
}

/// Formats events as logfmt lines.
#[derive(Debug, Clone, Copy)]
pub struct Logfmt {
    /// Whether to prefix each line with `ts=<RFC 3339>`.
    pub timestamps: bool,
}

impl<S, N> FormatEvent<S, N> for Logfmt
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut line = String::new();
        if self.timestamps {
            let now = jiff::Timestamp::now()
                .round(jiff::Unit::Second)
                .unwrap_or_else(|_| jiff::Timestamp::now());
            write!(line, "ts={now} ")?;
        }
        let level = match *event.metadata().level() {
            Level::ERROR => "error",
            Level::WARN => "warn",
            Level::INFO => "info",
            Level::DEBUG => "debug",
            Level::TRACE => "trace",
        };
        write!(line, "level={level}")?;
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        if let Some(message) = visitor.message {
            write!(line, " msg={}", quote(&message))?;
        }
        for (key, value) in visitor.fields {
            write!(line, " {key}={}", quote(&value))?;
        }
        writeln!(writer, "{line}")
    }
}

#[derive(Default)]
struct FieldVisitor {
    message: Option<String>,
    fields: Vec<(&'static str, String)>,
}

impl FieldVisitor {
    fn push(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = Some(value);
        } else {
            self.fields.push((field.name(), value));
        }
    }
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field, value.to_owned());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.push(field, value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.push(field, value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.push(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.push(field, format!("{value:?}"));
    }
}

/// Quotes a logfmt value if needed.
#[must_use]
pub fn quote(value: &str) -> String {
    let needs_quotes = value.is_empty()
        || value
            .chars()
            .any(|c| c == ' ' || c == '=' || c == '"' || c == '\\' || c.is_control());
    if !needs_quotes {
        return value.to_owned();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Whether stderr is connected to the systemd journal (`JOURNAL_STREAM`
/// matches stderr's device and inode).
#[must_use]
pub fn stderr_is_journal() -> bool {
    let Ok(value) = std::env::var("JOURNAL_STREAM") else {
        return false;
    };
    let Some((dev, ino)) = value.split_once(':') else {
        return false;
    };
    let (Ok(dev), Ok(ino)) = (dev.parse::<u64>(), ino.parse::<u64>()) else {
        return false;
    };
    rustix::fs::fstat(std::io::stderr().as_fd())
        .is_ok_and(|st| st.st_dev == dev && st.st_ino == ino)
}

/// Installs the global logfmt subscriber writing to stderr.
pub fn init(level: LogLevel) {
    let format = Logfmt {
        timestamps: !stderr_is_journal(),
    };
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(level.0)
        .with_writer(std::io::stderr)
        .event_format(format)
        .finish();
    // Ignore the error if a subscriber is already installed (tests).
    let _ = tracing::subscriber::set_global_default(subscriber);
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn parses_levels() {
        assert_eq!("WARN".parse::<LogLevel>().unwrap(), LogLevel(Level::WARN));
        assert!("verbose".parse::<LogLevel>().is_err());
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("ok"), "ok");
        assert_eq!(quote("/srv/a-b_c.d"), "/srv/a-b_c.d");
        assert_eq!(quote(""), "\"\"");
        assert_eq!(quote("two words"), "\"two words\"");
        assert_eq!(quote("a=b"), "\"a=b\"");
        assert_eq!(quote("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(quote("line\nbreak"), "\"line\\nbreak\"");
        assert_eq!(quote("back\\slash"), "\"back\\\\slash\"");
        assert_eq!(quote("bell\u{7}"), "\"bell\\u{7}\"");
    }

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn formats_events_as_logfmt() {
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(Level::INFO)
            .with_writer(move || writer.clone())
            .event_format(Logfmt { timestamps: false })
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(
                op = "reject",
                group = "research",
                name = ".hidden",
                reason = "name fails allowlist",
                count = 3u64,
                ok = true,
            );
            tracing::info!(target_dir = "/v", "privileges normalized");
            tracing::debug!("filtered out");
        });
        let out = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        assert_eq!(
            out,
            "level=warn op=reject group=research name=.hidden reason=\"name fails allowlist\" count=3 ok=true\n\
             level=info msg=\"privileges normalized\" target_dir=/v\n"
        );
    }

    #[test]
    fn timestamps_are_rfc3339() {
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .event_format(Logfmt { timestamps: true })
            .finish();
        tracing::subscriber::with_default(subscriber, || tracing::error!("x"));
        let out = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        let ts = out.strip_prefix("ts=").unwrap().split(' ').next().unwrap();
        assert!(ts.parse::<jiff::Timestamp>().is_ok(), "{out}");
        assert!(ts.ends_with('Z'));
    }

    #[test]
    fn journal_detection_requires_matching_stream() {
        // Not connected to the journal in tests unless the variable happens to
        // match.
        if std::env::var("JOURNAL_STREAM").is_err() {
            assert!(!stderr_is_journal());
        }
    }
}
