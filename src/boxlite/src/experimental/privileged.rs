//! Release-candidate privileged configuration.

/// Request the privileged OCI spec shape used by Docker-in-Docker.
///
/// The runtime must also enable
/// [`ExperimentalFeature::Privileged`](crate::experimental::ExperimentalFeature::Privileged).
/// This helper asks for the relaxed spec shape and the full capability set; the
/// host still treats those as separate policies when validating guest support.
pub fn configure(options: &mut crate::BoxOptions) {
    options.advanced.privileged = true;
    options.advanced.normalize_privileged();
}
