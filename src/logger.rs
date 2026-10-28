//! A minimal `log` backend: one line per record on stdout (journald adds its own
//! metadata). Our own modules log at the configured level; dependencies (TLS, HTTP)
//! only at WARN and above, so DEBUG never floods the journal with handshakes.

use std::io::Write;

use log::{Level, LevelFilter, Log, Metadata, Record};

struct Logger {
    level: LevelFilter,
    /// The CLI: no timestamps, and stderr, so stdout stays the command's output.
    cli: bool,
}

/// Python-style level names, so the config and the logs read the same as before.
fn level_name(level: Level) -> &'static str {
    match level {
        Level::Error => "ERROR",
        Level::Warn => "WARNING",
        Level::Info => "INFO",
        Level::Debug => "DEBUG",
        Level::Trace => "TRACE",
    }
}

pub fn parse_level(name: &str) -> LevelFilter {
    match name {
        "DEBUG" => LevelFilter::Debug,
        "WARNING" => LevelFilter::Warn,
        "ERROR" | "CRITICAL" => LevelFilter::Error,
        _ => LevelFilter::Info,
    }
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        let ours = metadata.target().starts_with("lolwatch");
        metadata.level() <= if ours { self.level } else { self.level.min(LevelFilter::Warn) }
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let target = record.target().replace("::", ".");
        let level = level_name(record.level());
        let _ = if self.cli {
            writeln!(std::io::stderr().lock(), "{level} {target}: {}", record.args())
        } else {
            let now = jiff::Zoned::now();
            let now = now.strftime("%Y-%m-%d %H:%M:%S,%3f");
            writeln!(std::io::stdout().lock(), "{now} {level} {target}: {}", record.args())
        };
    }

    fn flush(&self) {
        let _ = std::io::stdout().flush();
    }
}

/// Install once.
pub fn init(level: &str, cli: bool) {
    let level = parse_level(level);
    if log::set_boxed_logger(Box::new(Logger { level, cli })).is_ok() {
        log::set_max_level(level);
    }
}
