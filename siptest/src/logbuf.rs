//! A bounded ring of recent log lines, so an agent can diagnose without
//! locating the daemon's stderr — in practice the difference between
//! diagnosing a `403` and giving up (contracts/control-api.md).

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

const CAPACITY: usize = 500;

static BUFFER: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();

fn buffer() -> &'static Mutex<VecDeque<String>> {
    BUFFER.get_or_init(|| Mutex::new(VecDeque::with_capacity(CAPACITY)))
}

pub fn push(line: String) {
    let mut buf = buffer().lock().unwrap_or_else(|e| e.into_inner());
    if buf.len() >= CAPACITY {
        buf.pop_front();
    }
    buf.push_back(line);
}

/// The most recent `n` lines, oldest first.
pub fn tail(n: usize) -> Vec<String> {
    let buf = buffer().lock().unwrap_or_else(|e| e.into_inner());
    buf.iter().rev().take(n).rev().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    // The ring buffer is a process-global static, so tests that touch it
    // must not run concurrently with each other.
    static TEST_LOCK: StdMutex<()> = StdMutex::new(());

    #[test]
    fn tail_returns_the_most_recent_lines_in_order() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for i in 0..10 {
            push(format!("line-{i}"));
        }
        let last3 = tail(3);
        assert_eq!(last3, vec!["line-7", "line-8", "line-9"]);
    }

    #[test]
    fn asking_for_more_than_available_returns_what_exists() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for i in 0..3 {
            push(format!("only-{i}"));
        }
        assert!(tail(1000).len() >= 3);
    }
}
