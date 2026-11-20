//! Lightweight ring-buffered diagnostics logger for state transitions, errors
//! and crash reports. Never call from the 2.5 ms real-time packet hot path.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock, TryLockError};
use std::time::Instant;

const MAX_LOG_ENTRIES: usize = 128;

struct DiagStore {
    epoch: Instant,
    entries: Mutex<VecDeque<String>>,
}

fn store() -> &'static DiagStore {
    static STORE: OnceLock<DiagStore> = OnceLock::new();
    STORE.get_or_init(|| DiagStore {
        epoch: Instant::now(),
        entries: Mutex::new(VecDeque::with_capacity(MAX_LOG_ENTRIES)),
    })
}

fn push_entry(level: &str, component: &str, message: &str) {
    let s = store();
    let secs = s.epoch.elapsed().as_secs_f64();
    let line = format!("[+{secs:8.3}s] [{level}] [{component}] {message}");
    let mut guard = s.entries.lock().unwrap_or_else(|e| e.into_inner());
    if guard.len() >= MAX_LOG_ENTRIES {
        guard.pop_front();
    }
    guard.push_back(line);
}

/// Log an informational state transition.
pub fn info(component: &str, message: impl AsRef<str>) {
    push_entry("INFO", component, message.as_ref());
}

/// Log a recoverable error / fallback.
pub fn warn(component: &str, message: impl AsRef<str>) {
    push_entry("WARN", component, message.as_ref());
}

/// Log a fatal / unrecoverable condition.
pub fn error(component: &str, message: impl AsRef<str>) {
    push_entry("ERR ", component, message.as_ref());
}

/// Export a pasteable diagnostics bundle containing platform metadata,
/// caller-supplied live telemetry, and the last 128 state/error events.
pub fn export_report(live_summary: &str) -> String {
    let mut out = format!(
        "=== AudioShare Ultra Diagnostics ===\n\
         OS/Arch : {}/{}\n\
         Version : {}\n\
         {}\n\
         --- Event Log (newest last) ---\n",
        std::env::consts::OS,
        std::env::consts::ARCH,
        env!("CARGO_PKG_VERSION"),
        live_summary.trim()
    );
    let guard = store().entries.lock().unwrap_or_else(|e| e.into_inner());
    for line in guard.iter() {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Install a panic hook that writes the last diagnostic buffer + panic location
/// to `crash_log_path` before `panic = "abort"` terminates a release binary.
pub fn install_crash_hook(crash_log_path: std::path::PathBuf) {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        // Never block on the log mutex here. The panic may have fired while
        // this thread already held it (e.g. inside a formatting call), and a
        // second blocking lock() would deadlock before the crash log is
        // written. `try_lock` keeps the hook bounded; a poisoned lock is still
        // recovered so the buffered events survive.
        let s = store();
        let secs = s.epoch.elapsed().as_secs_f64();
        let line = format!("[+{secs:8.3}s] [ERR ] [panic] {panic_info}");
        match s.entries.try_lock() {
            Ok(mut guard) => {
                if guard.len() >= MAX_LOG_ENTRIES {
                    guard.pop_front();
                }
                guard.push_back(line);
            }
            Err(TryLockError::Poisoned(e)) => {
                let mut guard = e.into_inner();
                if guard.len() >= MAX_LOG_ENTRIES {
                    guard.pop_front();
                }
                guard.push_back(line);
            }
            Err(TryLockError::WouldBlock) => {}
        }

        let mut out = format!(
            "=== AudioShare Ultra Diagnostics ===\n\
             OS/Arch : {}/{}\n\
             Version : {}\n\
             {}\n\
             --- Event Log (newest last) ---\n",
            std::env::consts::OS,
            std::env::consts::ARCH,
            env!("CARGO_PKG_VERSION"),
            "Status  : FATAL PANIC".trim()
        );
        match s.entries.try_lock() {
            Ok(guard) => {
                for entry in guard.iter() {
                    out.push_str(entry);
                    out.push('\n');
                }
            }
            Err(TryLockError::Poisoned(e)) => {
                let guard = e.into_inner();
                for entry in guard.iter() {
                    out.push_str(entry);
                    out.push('\n');
                }
            }
            Err(TryLockError::WouldBlock) => {}
        }
        let _ = std::fs::write(&crash_log_path, out);
        prev(panic_info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diagnostics_ring_buffer_exports_events() {
        let report = export_report("Test: ring buffer");
        assert!(report.contains("OS/Arch"));
        assert!(report.contains("Test: ring buffer"));
        warn("test-component", "a recoverable fallback");
        let report = export_report("Test: after warn");
        assert!(report.contains("[WARN] [test-component] a recoverable fallback"));
    }

    #[test]
    fn test_diagnostics_ring_buffer_is_bounded() {
        for i in 0..MAX_LOG_ENTRIES + 32 {
            info("flood", format!("entry {i}"));
        }
        let report = export_report("");
        let lines = report.lines().filter(|l| l.contains("[INFO] [flood]")).count();
        assert!(lines <= MAX_LOG_ENTRIES, "ring overflowed: {lines} entries");
        assert!(!report.contains("entry 31")); // oldest entries evicted
        assert!(report.contains("entry 159")); // newest retained
    }
}
