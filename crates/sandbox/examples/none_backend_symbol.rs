//! CI assertion for D14 / kernel-interface §3.12: the `None` sandbox backend must be absent
//! from a default (release) build of the `sandbox` crate.
//!
//! This example references `sandbox::NoneBackend` by name, and its `[[example]]` entry in
//! `Cargo.toml` carries `required-features = ["dev-sandbox-none"]`. Cargo refuses to build an
//! explicitly requested target whose required features are missing, so the CI step is:
//!
//! ```text
//! # MUST fail: the feature is off, the symbol does not exist, cargo rejects the target.
//! ! cargo check -p sandbox --example none_backend_symbol
//! # MUST pass: with the feature the symbol exists.
//! cargo check -p sandbox --example none_backend_symbol --features dev-sandbox-none
//! ```
//!
//! A second, independent check that does not go through cargo's feature gate: build the library
//! without the feature and confirm the mangled symbol/metadata never mentions the type:
//!
//! ```text
//! cargo build -p sandbox --release
//! ! grep -q NoneBackend target/release/deps/libsandbox-*.rlib
//! ```

fn main() {
    // Only compiles when `sandbox::NoneBackend` exists, i.e. with `--features dev-sandbox-none`.
    println!("{}", std::any::type_name::<sandbox::NoneBackend>());
}
