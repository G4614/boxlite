// Copyright 2025 BoxLite Contributors
// SPDX-License-Identifier: Apache-2.0

//! Host-side Landlock handoff.
//!
//! The actual Landlock rule building and enforcement live in the
//! `boxlite-landlock` crate and are applied by its `libboxlite_landlock.so`
//! cdylib, which bwrap `LD_PRELOAD`s into the shim — so Landlock is enforced
//! inside the post-mount view, after bwrap finishes (a filesystem Landlock
//! domain denies every mount syscall, so it cannot be applied host-side before
//! bwrap). This module only serializes the rules into the environment for that
//! handoff.

pub use boxlite_landlock::{
    LANDLOCK_NETWORK_ENABLED_ENV, LANDLOCK_RULES_ENV, SEAL_MARKER_ENV, network_enabled_env_value,
};

/// Filename of the Landlock preload cdylib, co-located with the shim in the
/// runtime bundle. bwrap `LD_PRELOAD`s it into the shim via `--setenv`.
pub const SEAL_PRELOAD_LIB: &str = "libboxlite_landlock.so";

use crate::jailer::error::{IsolationError, JailerError};
use crate::jailer::sandbox::PathAccess;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};

/// Serialize path access rules for the preload library's env handoff.
pub fn serialize_rules_for_env(paths: &[PathAccess]) -> BoxliteResult<String> {
    boxlite_landlock::serialize_rules(paths)
        .map_err(|e| BoxliteError::from(JailerError::Isolation(IsolationError::Landlock(e.0))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn serialize_rules_for_env_round_trips_path_access() {
        let rules = vec![PathAccess {
            path: PathBuf::from("/tmp"),
            writable: true,
        }];
        let json = serialize_rules_for_env(&rules).unwrap();
        let decoded: Vec<PathAccess> = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].path, PathBuf::from("/tmp"));
        assert!(decoded[0].writable);
    }

    #[test]
    fn network_flag_encodes_one_zero() {
        assert_eq!(network_enabled_env_value(true), "1");
        assert_eq!(network_enabled_env_value(false), "0");
    }
}
