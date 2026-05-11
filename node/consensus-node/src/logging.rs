//! One-line structured event logger.
//!
//! Format: `[node N] view=V phase=PHASE event=NAME ts_ns=... K=V ...`
//! The harness in sun.sh / summarize-sweep.py greps these lines.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock went backwards")
        .as_nanos() as u64
}

pub fn log_event(node_id: u32, view: u64, event: &str, extras: serde_json::Value) {
    let mut extra_str = String::new();
    if let serde_json::Value::Object(map) = extras {
        for (k, v) in map {
            extra_str.push(' ');
            extra_str.push_str(&k);
            extra_str.push('=');
            match v {
                serde_json::Value::String(s) => extra_str.push_str(&s),
                other => extra_str.push_str(&other.to_string()),
            }
        }
    }
    println!(
        "[node {node_id}] view={view} {event} ts_ns={}{extra_str}",
        now_ns()
    );
}
