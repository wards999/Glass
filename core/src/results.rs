//! Session results between paired machines (protocol v16): a viewer opens a
//! results-only connection (`Hello` with [`CAP_RESULTS_ONLY`]) and the host
//! answers with its newest session logs, then closes: no session, no screen
//! change, no prompt. So one machine can read both ends of a session (and
//! the other machines' recent sessions) without anyone copying files.
//!
//! Each app names the folders its logs are in ([`add_dir`]): Windows its
//! `%LOCALAPPDATA%\Glass\logs` (and the service's), the Mac
//! `~/Library/Logs/Glass`. Only `.txt` files there are sent: Glass's own
//! results, nothing else on the machine.
//!
//! [`CAP_RESULTS_ONLY`]: crate::protocol::CAP_RESULTS_ONLY

use std::path::PathBuf;
use std::sync::Mutex;

/// The newest logs sent per request.
pub const NEWEST: usize = 8;
/// At most this much of each log (its end, where the totals are).
pub const PER_LOG: usize = 64 * 1024;

static DIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Adds a folder (or one file) whose `.txt` logs a results request gets.
pub fn add_dir(p: PathBuf) {
    let mut d = DIRS.lock().unwrap_or_else(|e| e.into_inner());
    if !d.contains(&p) {
        d.push(p);
    }
}

/// The newest [`NEWEST`] logs, newest first, each with a header line.
pub fn gather() -> String {
    let dirs = DIRS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut files = Vec::new();
    for d in dirs {
        if d.is_file() {
            files.push(d);
            continue;
        }
        if let Ok(rd) = std::fs::read_dir(&d) {
            files.extend(
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "txt")),
            );
        }
    }
    let modified = |p: &PathBuf| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH)
    };
    files.sort_by_key(|p| std::cmp::Reverse(modified(p)));
    files.dedup();
    let mut out = String::new();
    for p in files.iter().take(NEWEST) {
        let Ok(text) = std::fs::read_to_string(p) else {
            continue;
        };
        let ago = modified(p).elapsed().map_or(0, |d| d.as_secs());
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default();
        out.push_str(&format!(
            "===== {name} (written {} ago) =====\n",
            ago_text(ago)
        ));
        let start = text.len().saturating_sub(PER_LOG);
        let start = (start..text.len())
            .find(|&i| text.is_char_boundary(i))
            .unwrap_or(text.len());
        out.push_str(text[start..].trim_end());
        out.push_str("\n\n");
    }
    if out.is_empty() {
        out.push_str("(no session results on this machine yet)\n");
    }
    out
}

fn ago_text(s: u64) -> String {
    match s {
        0..=89 => format!("{s} s"),
        90..=5399 => format!("{} min", s / 60),
        5400..=172_799 => format!("{} h", s / 3600),
        _ => format!("{} days", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_logs_first_with_headers() {
        let d = std::env::temp_dir().join(format!("glass-results-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("viewer-1.txt"), "old\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(d.join("host-2.txt"), "new\n").unwrap();
        std::fs::write(d.join("notes.md"), "not a log").unwrap();
        add_dir(d.clone());
        let g = gather();
        let (a, b) = (
            g.find("host-2.txt").unwrap(),
            g.find("viewer-1.txt").unwrap(),
        );
        assert!(a < b, "{g}");
        assert!(!g.contains("not a log"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
