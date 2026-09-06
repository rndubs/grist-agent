//! The ACP protocol server (ADR-0004; `docs/specs/protocol.md`).
//!
//! - [`agent`]: the agent component every transport serves. `grist-kernel` connects it to stdio
//!   for exactly one session (D4); `grist-daemon` connects one per accepted unix-socket
//!   connection (D11) and lets it hold many sessions.
//! - [`session`]: the task that owns a `Kernel` and drives `run`.
//! - [`project`]: `Event`/`ModelDelta` → `session/update`.
//! - [`grist`]: the `_grist/*` extension messages.
//! - [`daemon`]: the supervisor behind `grist-daemon`: one `grist-kernel` process per session,
//!   proxied over the unix socket.
//! - [`connect`]: the stdio ↔ unix-socket forwarder behind `grist-connect`.

pub mod agent;
pub mod connect;
pub mod daemon;
pub mod grist;
pub mod project;
pub mod session;

pub use agent::{Server, ServerOptions, agent_component};
