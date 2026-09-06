//! `host` — `kernel::Host` implementations (P1.6): filesystem under an `FsPolicy` (D5), process
//! spawn with an exact environment (D10), network under a `NetPolicy`, secrets-as-handles with a
//! `SecretResolver` that registers every value with the shared `Redactor` (D10, §7.5), and
//! `ask_user` through a pluggable prompter (D17).
//!
//! - [`native::NativeHost`] is the in-process host used by the launcher in P1.
//! - [`remote_client::RemoteClientHost`] is the documented P3 stub.
//! - [`ask_user::AskUserTool`] is the `ask_user` tool (§7.10).
//! - [`endpoint_url_from_env`] implements the `GRIST_ENDPOINT_<NAME>_URL` rule of
//!   `profile-schema.md` §2.1 for the launcher.
//!
//! See `crates/host/README.md` for the policy-enforcement rules.

pub mod ask_user;
pub mod endpoint;
pub mod native;
pub mod prompt;
pub mod remote_client;
pub mod secrets;

pub use ask_user::AskUserTool;
pub use endpoint::{endpoint_env_var, endpoint_url_from_env, endpoint_url_from_lookup};
pub use native::{NativeHost, NetConfig, host_allowed};
pub use prompt::{ChannelPrompter, NoUserPrompter, PendingQuestion, UserPrompter};
pub use remote_client::RemoteClientHost;
pub use secrets::{EnvSecretSource, MapSecretSource, SecretSource};
