//! Shared test support: the sequential reference model and the
//! out-of-process crash runner.
//!
//! Each integration test crate compiles this module separately and uses a
//! different subset, so unused items are expected here and allowed once.
#![allow(dead_code)]

pub mod crash_child;
pub mod refdb;
