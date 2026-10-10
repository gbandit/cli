use std::io::Write;

use anyhow::{Context, Result, bail};
use chrono::Local;

/// `[Y/n]` prompt on stdout; empty input counts as yes.
pub(crate) fn confirm_yes(prompt: &str) -> Result<bool> {
    print!("{prompt}");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("failed to read confirmation")?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "" | "y" | "yes"
    ))
}

/// Type-the-exact-value friction for destructive actions: print `prompt`,
/// read a line, and bail with `mismatch_error` unless the trimmed input
/// matches `expected` verbatim.
pub(crate) fn confirm_typed(prompt: &str, expected: &str, mismatch_error: &str) -> Result<()> {
    print!("{prompt}");
    std::io::stdout().flush().ok();
    let mut typed = String::new();
    std::io::stdin()
        .read_line(&mut typed)
        .context("failed to read confirmation")?;
    if typed.trim() != expected {
        bail!("{mismatch_error}");
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) struct Printer {
    pub(crate) verbose: bool,
    pub(crate) timestamps: bool,
    pub(crate) json: bool,
}

impl Printer {
    /// CLI-side progress line. Always shown; timestamp follows the flag.
    pub(crate) fn progress(&self, msg: impl AsRef<str>) {
        let line = if self.timestamps {
            format!("{} {}", local_timestamp(), msg.as_ref())
        } else {
            msg.as_ref().to_string()
        };
        if self.json {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }

    /// Info-level Log event (headline): always shown.
    pub(crate) fn info(&self, stage: &str, created_at: Option<&str>, text: &str) {
        for line in text.lines() {
            self.print_event_line(stage, created_at, line, self.verbose);
        }
    }

    /// Debug-level Log event: shown only with `--verbose`.
    pub(crate) fn debug(&self, stage: &str, created_at: Option<&str>, text: &str) {
        if !self.verbose {
            return;
        }
        for line in text.lines() {
            self.print_event_line(stage, created_at, line, true);
        }
    }

    /// Status transition. Hidden in default mode; failed stages are surfaced
    /// separately with their buffered log.
    pub(crate) fn status(
        &self,
        stage: &str,
        created_at: Option<&str>,
        kind: &str,
        detail: Option<&str>,
    ) {
        if !self.verbose {
            return;
        }
        let line = match detail {
            Some(d) if !d.is_empty() => format!("{kind}: {d}"),
            _ => kind.to_string(),
        };
        self.print_event_line(stage, created_at, &line, true);
    }

    pub(crate) fn print_event_line(
        &self,
        stage: &str,
        created_at: Option<&str>,
        line: &str,
        with_label: bool,
    ) {
        let prefix_ts = if self.timestamps {
            created_at
                .and_then(format_event_timestamp)
                .unwrap_or_else(local_timestamp)
        } else {
            String::new()
        };
        let label = if with_label {
            format!("[{stage}] ")
        } else {
            String::new()
        };
        let rendered = if prefix_ts.is_empty() {
            format!("{label}{line}")
        } else {
            format!("{prefix_ts} {label}{line}")
        };
        if self.json {
            eprintln!("{rendered}");
        } else {
            println!("{rendered}");
        }
    }
}

/// `HH:MM:SS.cc` from the local wall clock.
fn local_timestamp() -> String {
    let now = Local::now();
    let cs = now.timestamp_subsec_millis() / 10;
    format!("{}.{cs:02}", now.format("%H:%M:%S"))
}

/// RFC3339 → `HH:MM:SS.cc` in local time. `None` on parse failure.
fn format_event_timestamp(created_at: &str) -> Option<String> {
    let parsed = chrono::DateTime::parse_from_rfc3339(created_at).ok()?;
    let local = parsed.with_timezone(&Local);
    let cs = local.timestamp_subsec_millis() / 10;
    Some(format!("{}.{cs:02}", local.format("%H:%M:%S")))
}

/// An RFC 3339 timestamp from the platform in the reader's local time, e.g.
/// "7 Nov 2026 at 14:03". Anything unparseable is shown as it came.
pub(crate) fn readable_time(rfc3339: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(rfc3339) {
        Ok(parsed) => parsed
            .with_timezone(&Local)
            .format("%-d %b %Y at %H:%M")
            .to_string(),
        Err(_) => rfc3339.to_string(),
    }
}
