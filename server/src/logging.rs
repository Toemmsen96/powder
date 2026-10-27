//! Logging: console + data/logs/server.log (rotated at 20 MB, 5 kept), in the format the launcher and
//! people already read: `HH:MM:SS.mmm LEVEL area         message`. Modules log with `log` targets
//! named "powder.<area>".

use log::{Level, LevelFilter, Log, Metadata, Record};
use parking_lot::Mutex;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_BYTES: u64 = 20_000_000;
const BACKUPS: usize = 5;

struct Logger {
    level: LevelFilter,
    file: Mutex<Option<RotatingFile>>,
}

struct RotatingFile {
    path: PathBuf,
    file: File,
    size: u64,
}

impl RotatingFile {
    fn open(path: PathBuf) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self { path, file, size })
    }

    fn write(&mut self, line: &[u8]) {
        if self.size + line.len() as u64 > MAX_BYTES {
            let _ = self.rotate();
        }
        if self.file.write_all(line).is_ok() {
            self.size += line.len() as u64;
        }
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        let name = |i: usize| PathBuf::from(format!("{}.{i}", self.path.display()));
        let _ = std::fs::remove_file(name(BACKUPS));
        for i in (1..BACKUPS).rev() {
            let _ = std::fs::rename(name(i), name(i + 1));
        }
        std::fs::rename(&self.path, name(1))?;
        *self = Self::open(self.path.clone())?;
        Ok(())
    }
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Error => "ERROR",
        Level::Warn => "WARNING",
        Level::Info => "INFO",
        Level::Debug => "DEBUG",
        Level::Trace => "TRACE",
    }
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level && metadata.target().starts_with("powder")
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let now = chrono::Local::now();
        let area = record.target().strip_prefix("powder.").unwrap_or(record.target());
        let line = format!("{} {:<5} {:<12} {}\n", now.format("%H:%M:%S%.3f"), level_name(record.level()), area,
                           record.args());
        let _ = std::io::stderr().write_all(line.as_bytes());
        if let Some(f) = self.file.lock().as_mut() {
            f.write(line.as_bytes());
        }
    }

    fn flush(&self) {
        if let Some(f) = self.file.lock().as_mut() {
            let _ = f.file.flush();
        }
    }
}

pub fn parse_level(level: &str) -> LevelFilter {
    match level.to_ascii_uppercase().as_str() {
        "DEBUG" => LevelFilter::Debug,
        "TRACE" => LevelFilter::Trace,
        "WARNING" | "WARN" => LevelFilter::Warn,
        "ERROR" | "CRITICAL" => LevelFilter::Error,
        _ => LevelFilter::Info,
    }
}

/// Console + rotating file in `log_dir`. Safe to call once per process; later calls are ignored.
pub fn setup(log_dir: Option<&Path>, level: &str) {
    let level = parse_level(level);
    let file = log_dir.and_then(|dir| {
        std::fs::create_dir_all(dir).ok()?;
        RotatingFile::open(dir.join("server.log")).ok()
    });
    let logger = Box::new(Logger { level, file: Mutex::new(file) });
    if log::set_boxed_logger(logger).is_ok() {
        log::set_max_level(level);
    }
}
