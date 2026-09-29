//! ME4-5.2.1 migration (`docs/design/ME4-S3-os-sdk.md` §5.1/H4): this file
//! used to hand-roll the fake-kernel plumbing (`fake_kernel`/`read_request`/
//! `respond`/`respond_error`/`respond_error_with_data`/`FakePeer`) the four
//! typed client test suites and `reconciler.rs`'s own test module share.
//! `agent24-os-sdk`'s `testing` module (behind its `test-util` feature)
//! ships the exact same function names and signatures (H4: this crate's own
//! `test_support.rs` was mirrored back into the SDK precisely so this shim
//! could be a single `pub(crate) use` line) — every caller across this
//! crate's test modules, including `reconciler.rs`'s (which must survive
//! this migration BYTE-IDENTICAL, §5.2 point 3), keeps working unchanged.

#![cfg(test)]

pub(crate) use agent24_os_sdk::testing::{
    fake_kernel, read_request, respond, respond_error, respond_error_with_data, FakePeer,
};
