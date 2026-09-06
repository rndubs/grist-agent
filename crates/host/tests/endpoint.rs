//! `GRIST_ENDPOINT_<NAME>_URL` (profile-schema.md §2.1).

use host::{endpoint_env_var, endpoint_url_from_env, endpoint_url_from_lookup};
use kernel::HostError;

#[test]
fn env_var_mapping() {
    assert_eq!(
        endpoint_env_var("litellm-ci"),
        "GRIST_ENDPOINT_LITELLM_CI_URL"
    );
    assert_eq!(endpoint_env_var("vllm"), "GRIST_ENDPOINT_VLLM_URL");
    assert_eq!(endpoint_env_var("a-b-c1"), "GRIST_ENDPOINT_A_B_C1_URL");
}

#[test]
fn lookup_resolution() {
    let vars = |k: &str| {
        (k == "GRIST_ENDPOINT_LITELLM_CI_URL").then(|| " http://litellm:4000/v1 ".to_owned())
    };
    assert_eq!(
        endpoint_url_from_lookup("litellm-ci", vars).unwrap(),
        "http://litellm:4000/v1"
    );
    assert!(matches!(
        endpoint_url_from_lookup("vllm", vars),
        Err(HostError::NotFound(m)) if m.contains("GRIST_ENDPOINT_VLLM_URL") && m.contains("not set")
    ));
    assert!(matches!(
        endpoint_url_from_lookup("x", |_| Some("  ".to_owned())),
        Err(HostError::NotFound(m)) if m.contains("empty")
    ));
}

#[test]
fn env_resolution_reports_missing_variable() {
    // Tests cannot set environment variables (`unsafe_code = forbid`), so only the missing case
    // is checked against the live environment; the positive path is `lookup_resolution`.
    let r = endpoint_url_from_env("grist-test-surely-unset");
    assert!(
        matches!(
            r,
            Err(HostError::NotFound(ref m)) if m.contains("GRIST_ENDPOINT_GRIST_TEST_SURELY_UNSET_URL")
        ),
        "{r:?}"
    );
}
