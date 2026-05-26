//! Scenario registry — each Phase appends to [`registry()`] and the
//! dispatcher in [`build_by_name`] without touching the runner
//! plumbing.
//!
//! The two functions are kept in lock-step by hand: a `match` arm in
//! [`build_by_name`] for every entry in [`registry`]. A test below
//! pins that invariant so it can't silently drift when a new
//! scenario lands.

use super::runner::Scenario;

/// One row in `boxlite bench list`. The registry is intentionally
/// static-data (vs. a `Box<dyn Fn>` table) so listing has zero
/// construction cost — building the actual scenario happens on
/// `run`, lazily, in [`build_by_name`].
pub struct ScenarioEntry {
    pub name: &'static str,
    pub description: &'static str,
}

/// All registered scenarios, in display order.
pub fn registry() -> &'static [ScenarioEntry] {
    &[]
}

/// Construct a scenario by name. Returns `None` if the name doesn't
/// match any entry in [`registry()`]; the runner converts that into a
/// user-facing "unknown scenario" error.
pub fn build_by_name(_name: &str) -> Option<Box<dyn Scenario>> {
    // Per-axis batches append match arms here as their scenario
    // modules land.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `registry()` and `build_by_name()` are kept in lock-step by
    /// hand. This test enforces that contract — every entry in the
    /// registry must be constructible by name. A new scenario added
    /// to the registry without a matching arm here will surface as a
    /// missing-arm failure instead of a runtime "unknown scenario"
    /// error.
    #[test]
    fn every_registered_scenario_is_buildable() {
        for entry in registry() {
            let built = build_by_name(entry.name);
            assert!(
                built.is_some(),
                "registry entry {:?} has no build_by_name arm",
                entry.name
            );
            // Round-trip: built scenario's name() must match.
            let s = built.unwrap();
            assert_eq!(
                s.name(),
                entry.name,
                "build_by_name({:?}) returned a scenario reporting name() = {:?}",
                entry.name,
                s.name()
            );
        }
    }

    /// Names are user-facing; pin uniqueness in the registry so a
    /// rename accident can't collide with an existing scenario and
    /// silently overwrite it.
    #[test]
    fn registry_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for entry in registry() {
            assert!(
                seen.insert(entry.name),
                "duplicate scenario name in registry: {:?}",
                entry.name
            );
        }
    }
}
