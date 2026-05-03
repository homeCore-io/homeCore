//! Custom rotating log file writer for hc-logging.
//!
//! Replaces `tracing_appender::rolling::RollingFileAppender` with a writer
//! that supports:
//!
//! - **Time-based rotation**: hourly, daily, weekly, or never
//! - **Size-based rotation**: rotate when the active file exceeds `max_bytes`
//! - **Combined**: "daily OR 100 MB, whichever comes first"
//! - **Compression**: rotated files are gzip-compressed in a background thread
//!
//! # File naming
//!
//! | File | Path |
//! |---|---|
//! | Active (currently written) | `{dir}/{prefix}.log` |
//! | First rotation in a period | `{dir}/{prefix}.{period}.log[.gz]` |
//! | Nth size rotation same period | `{dir}/{prefix}.{period}.N.log[.gz]` |
//!
//! Period format: `2026-03-27` (daily), `2026-03-27_14` (hourly),
//! `2026-W13` (weekly).  `Never` strategy uses a full timestamp
//! (`2026-03-27T142501`) so each size-triggered rotation gets a unique name.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::config::RotationStrategy;

// ---------------------------------------------------------------------------
// Public writer
// ---------------------------------------------------------------------------

/// A file writer that rotates based on time, size, or both.
///
/// Implements `std::io::Write`; pass to `tracing_appender::non_blocking` for
/// async, non-blocking log dispatch.
pub struct RotatingWriter {
    file: File,
    /// Bytes written to the current active file.
    bytes_written: u64,
    /// Rotate when `bytes_written >= max_bytes`.  `0` = no size limit.
    max_bytes: u64,
    rotation: RotationStrategy,
    /// The period string that was current when `file` was opened.
    /// Used to detect when the period rolls over.  Empty for `Never`.
    current_period: String,
    dir: PathBuf,
    prefix: String,
    /// When `true`, spawn a background thread to gzip each rotated file.
    compress: bool,
    /// How many size-triggered rotations have happened in `current_period`.
    /// Used to generate unique suffixes (`.1`, `.2`, …).
    period_counter: u32,
    /// Delete rotated files older than this many days.  `0` = never prune.
    prune_after_days: u32,
}

impl RotatingWriter {
    /// Open (or create) the active log file and return a ready writer.
    ///
    /// If the active file already exists (e.g. after a restart), its current
    /// size is used as the initial `bytes_written` so that a file already near
    /// the size limit will rotate promptly.
    pub fn new(
        dir: PathBuf,
        prefix: String,
        rotation: RotationStrategy,
        max_bytes: u64,
        compress: bool,
        prune_after_days: u32,
    ) -> io::Result<Self> {
        let current_period = period_str(&rotation);
        let active = active_path(&dir, &prefix);
        let file = open_append(&active)?;
        let bytes_written = file.metadata().map(|m| m.len()).unwrap_or(0);

        let writer = Self {
            file,
            bytes_written,
            max_bytes,
            rotation,
            current_period,
            dir,
            prefix,
            compress,
            period_counter: 0,
            prune_after_days,
        };

        // Run an initial prune at startup.
        if prune_after_days > 0 {
            prune_old_logs(&writer.dir, &writer.prefix, prune_after_days);
        }

        Ok(writer)
    }

    fn maybe_rotate(&mut self) -> io::Result<()> {
        let new_period = period_str(&self.rotation);
        let period_changed = !new_period.is_empty() && new_period != self.current_period;
        let size_exceeded = self.max_bytes > 0 && self.bytes_written >= self.max_bytes;

        if !period_changed && !size_exceeded {
            return Ok(());
        }

        self.file.flush()?;

        if period_changed {
            self.period_counter = 0;
            self.current_period = new_period;
        }

        let rotated = self.next_rotated_path();
        let active = active_path(&self.dir, &self.prefix);

        std::fs::rename(&active, &rotated)?;

        if self.compress {
            compress_in_background(rotated);
        }

        self.file = open_append(&active)?;
        self.bytes_written = 0;
        self.period_counter += 1;

        if self.prune_after_days > 0 {
            prune_old_logs(&self.dir, &self.prefix, self.prune_after_days);
        }

        Ok(())
    }

    /// Return the next available path for the file being rotated out.
    fn next_rotated_path(&self) -> PathBuf {
        // For `Never` strategy, use a full timestamp so every rotation is unique.
        let period = if self.current_period.is_empty() {
            hc_time::now_local().format("%Y-%m-%dT%H%M%S").to_string()
        } else {
            self.current_period.clone()
        };

        // First rotation in this period: try without a numeric suffix.
        if self.period_counter == 0 {
            let candidate = self.dir.join(format!("{}.{}.log", self.prefix, period));
            if !candidate.exists() {
                return candidate;
            }
        }

        // Find the first unused numeric suffix.
        let start = if self.period_counter == 0 {
            1
        } else {
            self.period_counter
        };
        let mut n = start;
        loop {
            let candidate = self
                .dir
                .join(format!("{}.{}.{}.log", self.prefix, period, n));
            if !candidate.exists() {
                return candidate;
            }
            n += 1;
        }
    }
}

impl Write for RotatingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.maybe_rotate()?;
        let n = self.file.write(buf)?;
        self.bytes_written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn active_path(dir: &Path, prefix: &str) -> PathBuf {
    dir.join(format!("{}.log", prefix))
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// Compute the current time-period string for the given rotation strategy.
/// Returns an empty string for `Never` (time-based rotation disabled).
fn period_str(rotation: &RotationStrategy) -> String {
    let now = hc_time::now_local();
    match rotation {
        RotationStrategy::Hourly => now.format("%Y-%m-%d_%H").to_string(),
        RotationStrategy::Daily => now.format("%Y-%m-%d").to_string(),
        RotationStrategy::Weekly => now.format("%Y-W%V").to_string(),
        RotationStrategy::Never => String::new(),
    }
}

/// Delete rotated log files for `prefix` in `dir` that are older than
/// `max_age_days`.  Matches files like `prefix.*.log`, `prefix.*.log.gz`.
/// The active file (`prefix.log`) is never deleted.
fn prune_old_logs(dir: &Path, prefix: &str, max_age_days: u32) {
    let cutoff = std::time::SystemTime::now()
        - std::time::Duration::from_secs(u64::from(max_age_days) * 86_400);

    let rotated_prefix = format!("{}.", prefix);
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();

        // Must be a rotated file for this prefix (not the active file).
        if !name.starts_with(&rotated_prefix) {
            continue;
        }
        if !name.ends_with(".log") && !name.ends_with(".log.gz") {
            continue;
        }

        let modified = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };

        if modified < cutoff {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Gzip `src` → `src.gz` in a background thread, then delete `src` on success.
fn compress_in_background(src: PathBuf) {
    std::thread::spawn(move || {
        let mut gz_os = src.as_os_str().to_owned();
        gz_os.push(".gz");
        let gz_path = PathBuf::from(gz_os);

        let result: io::Result<()> = (|| {
            use flate2::{write::GzEncoder, Compression};
            let mut input = File::open(&src)?;
            let output = File::create(&gz_path)?;
            let mut encoder = GzEncoder::new(output, Compression::default());
            io::copy(&mut input, &mut encoder)?;
            encoder.finish()?;
            drop(input);
            std::fs::remove_file(&src)?;
            Ok(())
        })();

        if let Err(e) = result {
            eprintln!("hc-logging: compression failed for {:?}: {e}", src);
        }
    });
}
