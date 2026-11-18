//! Fuzz target: see `kwms::protocol::fuzz_harness::check_kvp` for the
//! invariants checked on every input.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Requests reach the protocol layer as &str (axum rejects non-UTF-8).
    if let Ok(input) = std::str::from_utf8(data) {
        kwms::protocol::fuzz_harness::check_kvp(input);
    }
});
