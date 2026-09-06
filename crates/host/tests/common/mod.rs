//! Shared test helpers.
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use host::{MapSecretSource, NativeHost, NetConfig};
use kernel::{
    CancellationToken, Command, FsMode, FsPolicy, Mount, NetAllow, NetPolicy, ProcPolicy,
    ProcessOutput, Redactor, SandboxBackend, SandboxError, SandboxPolicy, SessionProcess,
};

/// A host with an empty secret map, no user, and a proxy-free network config.
pub fn host() -> NativeHost {
    NativeHost::new(
        Arc::new(Redactor::new()),
        Arc::new(MapSecretSource::empty()),
    )
    .with_net_config(NetConfig::default().without_proxy())
}

pub fn mount(path: &Path, mode: FsMode) -> Mount {
    Mount {
        path: path.to_path_buf(),
        mode,
    }
}

pub fn fs_policy(mounts: Vec<Mount>) -> FsPolicy {
    FsPolicy { mounts }
}

pub fn proc_policy(programs: &[&str], timeout: Duration) -> ProcPolicy {
    ProcPolicy {
        programs: programs.iter().map(|s| (*s).to_owned()).collect(),
        env_allowlist: BTreeSet::new(),
        timeout,
    }
}

pub fn sandbox_policy() -> SandboxPolicy {
    SandboxPolicy {
        mounts: Vec::new(),
        scratch_tmpfs_mb: 1,
        net: NetPolicy {
            enabled: false,
            allow: NetAllow::Hosts(BTreeSet::new()),
        },
        timeout: Duration::from_secs(5),
        env_allowlist: BTreeSet::new(),
        programs: BTreeSet::new(),
    }
}

/// A backend that refuses every launch (the tools under test never sandbox anything).
pub struct FakeSandbox;

#[async_trait]
impl SandboxBackend for FakeSandbox {
    fn name(&self) -> &'static str {
        "none"
    }

    async fn launch_stateless(
        &self,
        _policy: &SandboxPolicy,
        _cmd: Command,
        _cancel: CancellationToken,
    ) -> Result<ProcessOutput, SandboxError> {
        Err(SandboxError::Launch("fake".to_owned()))
    }

    async fn launch_session(
        &self,
        _policy: &SandboxPolicy,
        _cmd: Command,
    ) -> Result<Box<dyn SessionProcess>, SandboxError> {
        Err(SandboxError::Launch("fake".to_owned()))
    }
}
