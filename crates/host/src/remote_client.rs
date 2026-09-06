//! `host::remote-client` (P3.3 stub). Compiles; every method fails with
//! `HostError::Io("remote-client host is not implemented until P3")`.
//!
//! # The P3 shape
//!
//! A kernel placed on an HPC login node (D18) runs in a container whose filesystem is not the
//! user's laptop. `RemoteClientHost` implements the same [`kernel::Host`] trait, but every call is
//! proxied over the P1.9 unix-socket protocol (SSH-forwarded, D11) to a *host process* running on
//! the login node outside the container, so the kernel's `read`/`write`/`edit`, `spawn`,
//! `network`, and `ask_user` reach the right filesystem, the right network egress, and the human
//! at the client:
//!
//! - `read_file`/`write_file`/`list_dir`/`stat`/`remove` become `host/fs.*` requests carrying the
//!   `FsPolicy`; the host process re-runs the same path resolution and policy check as
//!   [`crate::native::NativeHost`] (the policy is enforced on the side that touches the disk, never
//!   trusted from the wire).
//! - `spawn` becomes `host/proc.spawn` returning a remote pid; `ChildProcess::wait`/`terminate`
//!   are `host/proc.wait` and `host/proc.terminate` with the same SIGTERM → SIGKILL escalation.
//! - `network` returns a `NetHandle` that forwards `host/net.send` and streams body chunks back
//!   as notifications; the host process applies the `NetAllow` check.
//! - `secret` asks the host process for a handle (name only); `SecretResolver` is **not**
//!   implemented by this type: secret values never cross the socket. Provider clients on a remote
//!   kernel resolve through the host process's own resolver (P3.3 decides the exact mechanism).
//! - `ask_user` forwards the `AskUserRequest` to the client and awaits the `UserAnswer`.
//!
//! The connection, request ids, and reconnection are P3.3 concerns; this type has no fields yet.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use kernel::{
    AskUserRequest, ChildProcess, Command, DirEntry, FsPolicy, Host, HostError, Metadata,
    NetHandle, NetPolicy, ProcPolicy, SecretHandle, UserAnswer,
};

/// The message every method fails with until P3.3.
pub const NOT_IMPLEMENTED: &str = "remote-client host is not implemented until P3";

/// A `Host` whose calls are proxied to a host process on the login node (P3.3). Stub.
#[derive(Clone, Copy, Debug, Default)]
pub struct RemoteClientHost;

impl RemoteClientHost {
    /// The stub. Takes no connection yet.
    pub fn new() -> RemoteClientHost {
        RemoteClientHost
    }

    fn unimplemented<T>() -> Result<T, HostError> {
        Err(HostError::Io(NOT_IMPLEMENTED.to_owned()))
    }
}

#[async_trait]
impl Host for RemoteClientHost {
    fn name(&self) -> &str {
        "remote-client"
    }

    async fn read_file(&self, _policy: &FsPolicy, _path: &Path) -> Result<Vec<u8>, HostError> {
        Self::unimplemented()
    }

    async fn write_file(
        &self,
        _policy: &FsPolicy,
        _path: &Path,
        _bytes: &[u8],
    ) -> Result<(), HostError> {
        Self::unimplemented()
    }

    async fn list_dir(&self, _policy: &FsPolicy, _path: &Path) -> Result<Vec<DirEntry>, HostError> {
        Self::unimplemented()
    }

    async fn stat(&self, _policy: &FsPolicy, _path: &Path) -> Result<Metadata, HostError> {
        Self::unimplemented()
    }

    async fn remove(&self, _policy: &FsPolicy, _path: &Path) -> Result<(), HostError> {
        Self::unimplemented()
    }

    async fn spawn(
        &self,
        _policy: &ProcPolicy,
        _cmd: Command,
    ) -> Result<Box<dyn ChildProcess>, HostError> {
        Self::unimplemented()
    }

    fn network(&self, _policy: &NetPolicy) -> Result<Arc<dyn NetHandle>, HostError> {
        Self::unimplemented()
    }

    fn secret(&self, _name: &str) -> Result<SecretHandle, HostError> {
        Self::unimplemented()
    }

    async fn ask_user(&self, _req: AskUserRequest) -> Result<UserAnswer, HostError> {
        Self::unimplemented()
    }
}
