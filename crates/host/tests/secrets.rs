//! Secrets as handles (D10, §7.5): handles carry no value, resolution registers with the
//! redactor, and nothing prints a value.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use host::{EnvSecretSource, MapSecretSource, NativeHost};
use kernel::{Host, HostError, Redactor, SecretHandle, SecretResolver};

const VALUE: &str = "sk-live-0123456789abcdefXYZ";

fn map_host() -> (Arc<Redactor>, NativeHost) {
    let redactor = Arc::new(Redactor::new());
    let host = NativeHost::new(
        Arc::clone(&redactor),
        Arc::new(MapSecretSource::new([("API_KEY", VALUE)])),
    );
    (redactor, host)
}

#[test]
fn secret_returns_a_handle_without_the_value() {
    let (_, host) = map_host();
    let handle = host.secret("API_KEY").unwrap();
    assert_eq!(handle.name(), "API_KEY");
    assert_eq!(handle.locator(), Some("map:API_KEY"));
    let dbg = format!("{handle:?}");
    assert!(!dbg.contains(VALUE), "{dbg}");
    let json = serde_json::to_string(&handle).unwrap();
    assert!(!json.contains(VALUE), "{json}");
    assert!(!json.contains("map:"), "locator must not serialize: {json}");
}

#[test]
fn unknown_secret_errors_on_handle_and_on_resolve() {
    let (_, host) = map_host();
    assert!(matches!(
        host.secret("NOPE"),
        Err(HostError::UnknownSecret(n)) if n == "NOPE"
    ));
    assert!(matches!(
        host.resolve_secret(&SecretHandle::new("NOPE")),
        Err(HostError::UnknownSecret(n)) if n == "NOPE"
    ));
}

#[test]
fn resolve_registers_the_value_with_the_redactor() {
    let (redactor, host) = map_host();
    let probe = format!("Authorization: {VALUE} done");
    // Not registered yet: the value survives (the built-in `sk-` pattern does not match this shape).
    assert_eq!(redactor.known_count(), 0);

    let handle = host.secret("API_KEY").unwrap();
    let value = host.resolve_secret(&handle).unwrap();
    assert_eq!(value.expose(), VALUE);
    assert_eq!(redactor.known_count(), 1);
    let (out, report) = redactor.redact_str(&probe);
    assert_eq!(out, "Authorization: [REDACTED:secret] done");
    assert_eq!(report.replacements, 1);

    // A handle without a locator (deserialized) resolves too, and registration is idempotent.
    host.resolve_secret(&SecretHandle::new("API_KEY")).unwrap();
    assert_eq!(redactor.known_count(), 1);
}

#[test]
fn debug_of_host_and_sources_never_prints_values() {
    let (_, host) = map_host();
    let dbg = format!("{host:?}");
    assert!(dbg.contains("API_KEY"), "{dbg}");
    assert!(!dbg.contains(VALUE), "{dbg}");
    let value = host
        .resolve_secret(&host.secret("API_KEY").unwrap())
        .unwrap();
    assert_eq!(format!("{value:?}"), "SecretString(***)");
    let src = EnvSecretSource::from_snapshot(BTreeMap::from([("K".to_owned(), VALUE.to_owned())]));
    let dbg = format!("{src:?}");
    assert!(dbg.contains("\"K\"") && !dbg.contains(VALUE), "{dbg}");
}

#[test]
fn env_source_is_a_snapshot_taken_at_construction() {
    // Cargo sets CARGO_PKG_NAME for every test process.
    let host = NativeHost::new(
        Arc::new(Redactor::new()),
        Arc::new(EnvSecretSource::from_env()),
    );
    let handle = host.secret("CARGO_PKG_NAME").unwrap();
    assert_eq!(handle.locator(), Some("env:CARGO_PKG_NAME"));
    assert_eq!(host.resolve_secret(&handle).unwrap().expose(), "host");
    assert!(matches!(
        host.secret("GRIST_TEST_SURELY_UNSET_VARIABLE"),
        Err(HostError::UnknownSecret(_))
    ));

    let snap = EnvSecretSource::from_snapshot(BTreeMap::from([(
        "ONLY_IN_SNAPSHOT".to_owned(),
        "0123456789".to_owned(),
    )]));
    assert_eq!(snap.names().collect::<Vec<_>>(), vec!["ONLY_IN_SNAPSHOT"]);
    let host = NativeHost::new(Arc::new(Redactor::new()), Arc::new(snap));
    assert!(host.secret("ONLY_IN_SNAPSHOT").is_ok());
    assert!(host.secret("CARGO_PKG_NAME").is_err());
}
