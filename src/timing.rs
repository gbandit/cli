//! `GBANDIT_TIMING=1` prints `@timing phase=<name> at=<unix ms> ms=<duration>`
//! to stderr when a phase of a command ends. `deploy_benchmark.py` places
//! them on the same axis as the platform's Timeline Events.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub(crate) struct Phase {
    name: &'static str,
    started_at_unix_ms: u128,
    started: Instant,
}

pub(crate) fn start(name: &'static str) -> Phase {
    Phase {
        name,
        started_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0),
        started: Instant::now(),
    }
}

impl Phase {
    pub(crate) fn end(self) {
        if std::env::var_os("GBANDIT_TIMING").is_some() {
            eprintln!(
                "@timing phase={} at={} ms={}",
                self.name,
                self.started_at_unix_ms,
                self.started.elapsed().as_millis()
            );
        }
    }
}
