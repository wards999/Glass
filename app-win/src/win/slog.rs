//! Running session logs: while a session is on, its results are rewritten
//! here every 10 s, so a session that ends abruptly (Glass killed by
//! antivirus, a crash, a power cut) still leaves its numbers. m4-test1:
//! Defender removed Glass.exe mid-session and nothing was left.
//!
//! `%LOCALAPPDATA%\Glass\logs\<kind>-<unix seconds>.txt`, not the program's
//! folder (which can be removed or quarantined). The newest 20 are kept.

use std::path::PathBuf;

/// How often a running session's log is rewritten (µs).
pub const EVERY_US: u64 = 10_000_000;
const KEEP: usize = 20;

pub fn dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Glass").join("logs")
}

/// One session's log file.
pub struct SessionLog {
    path: PathBuf,
}

impl SessionLog {
    /// A new log for a `kind` session ("viewer", "host").
    pub fn start(kind: &str) -> Self {
        let d = dir();
        let _ = std::fs::create_dir_all(&d);
        prune(&d);
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |t| t.as_secs());
        Self {
            path: d.join(format!("{kind}-{secs}.txt")),
        }
    }

    /// Replaces the log with `text` (written whole, then renamed into
    /// place, so it's never half-written).
    pub fn write(&self, text: &str) {
        let tmp = self.path.with_extension("tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

fn logs(d: &std::path::Path) -> Vec<(std::time::SystemTime, PathBuf)> {
    let mut v: Vec<_> = std::fs::read_dir(d)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "txt"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    v.sort();
    v
}

fn prune(d: &std::path::Path) {
    let v = logs(d);
    for (_, p) in v.iter().take(v.len().saturating_sub(KEEP - 1)) {
        let _ = std::fs::remove_file(p);
    }
}

/// The newest session log, if any.
pub fn latest() -> Option<String> {
    let (_, p) = logs(&dir()).pop()?;
    std::fs::read_to_string(p).ok()
}
