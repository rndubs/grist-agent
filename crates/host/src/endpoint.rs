//! Endpoint base-URL resolution for the launcher (`profile-schema.md` §2.1): the profile names an
//! endpoint; the URL comes from `GRIST_ENDPOINT_<NAME>_URL` with `<NAME>` upper-cased and `-`
//! mapped to `_`. A missing variable is a fatal start error, not a validator error.

use kernel::HostError;

/// The environment variable holding the base URL of `endpoint`
/// (`litellm-ci` → `GRIST_ENDPOINT_LITELLM_CI_URL`).
pub fn endpoint_env_var(endpoint: &str) -> String {
    format!(
        "GRIST_ENDPOINT_{}_URL",
        endpoint.to_ascii_uppercase().replace('-', "_")
    )
}

/// Resolve `endpoint` through `lookup` (a map of environment variables, or `std::env::var`).
/// `NotFound` when the variable is absent or blank.
pub fn endpoint_url_from_lookup(
    endpoint: &str,
    lookup: impl FnOnce(&str) -> Option<String>,
) -> Result<String, HostError> {
    let var = endpoint_env_var(endpoint);
    match lookup(&var) {
        Some(v) if !v.trim().is_empty() => Ok(v.trim().to_owned()),
        Some(_) => Err(HostError::NotFound(format!(
            "environment variable `{var}` (endpoint `{endpoint}`) is empty"
        ))),
        None => Err(HostError::NotFound(format!(
            "environment variable `{var}` (endpoint `{endpoint}`) is not set"
        ))),
    }
}

/// Resolve `endpoint` from the live process environment.
pub fn endpoint_url_from_env(endpoint: &str) -> Result<String, HostError> {
    let var = endpoint_env_var(endpoint);
    match std::env::var(&var) {
        Ok(v) => endpoint_url_from_lookup(endpoint, |_| Some(v)),
        Err(std::env::VarError::NotPresent) => endpoint_url_from_lookup(endpoint, |_| None),
        Err(std::env::VarError::NotUnicode(_)) => Err(HostError::Io(format!(
            "environment variable `{var}` (endpoint `{endpoint}`) is not valid UTF-8"
        ))),
    }
}
