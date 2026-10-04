//! `ext` — Extension API and first-party extensions: MCP client, sub-agent spawn, skills loader,
//! memory modules, workflow runner, Python REPL tool, capability gate.
//!
//! P2.1 lands the two extension tiers of D8 and ADR-0001:
//!
//! - **Compiled tier** ([`compiled`]): Rust middleware and first-party tools implement
//!   [`Extension`] and are collected in an [`ExtensionSet`], which hands the launcher the
//!   session's tools, their declarations for the profiles validator, and middleware by name. The
//!   six base tools are registered through it as [`BaseTools`]. The agent never authors this tier.
//! - **Out-of-process tier** ([`manifest`], [`loader`], [`process`]): an agent-authorable tool is
//!   a directory with an `extension.toml` manifest (name, version, kind, command, capabilities
//!   required, tools provided). [`load_all`] parses the manifests a profile names and admits each
//!   one only if the profile's grants cover every capability it requires; its tools then run as
//!   `ext.<extension>.<tool>` under the existing Stateless or Session sandbox launcher, speaking
//!   the stdio subset of MCP (`tools/call`).
//!
//! Specs: `docs/specs/extension-manifest.md`; the profile key is `[extensions].paths`
//! (`docs/specs/profile-schema.md` §3.12). Example: `examples/text-stats/`.

pub mod compiled;
pub mod error;
pub mod loader;
pub mod manifest;
pub mod process;

pub use compiled::{BaseTools, Extension, ExtensionSet, RESERVED_TOOL_PREFIXES, SessionSetup};
pub use error::ExtError;
pub use loader::{Loaded, ProbeReport, admit, load_all, policy_for, probe};
pub use manifest::{MANIFEST_FILE, Manifest, Placeholders, ToolSpec};
pub use process::ProcessTool;

/// The MCP protocol revision sent in `initialize` by [`probe`] (the wire is a strict subset of
/// MCP's stdio transport, `docs/specs/extension-manifest.md` §3).
pub const PROTOCOL_VERSION: &str = "2025-06-18";
