//! `NativeHost`: the in-process `Host` (P1). Filesystem calls are policy-checked in process (D5),
//! spawned children get exactly the command's environment (D10), network access is gated by
//! `NetPolicy`, secrets are handles, and every resolved value is registered with the shared
//! `Redactor` before it is returned (§7.5).

mod fs;
mod net;
mod process;

use std::fmt;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use kernel::{
    AskUserRequest, ChildProcess, Command, DirEntry, FsMode, FsPolicy, Host, HostError, Metadata,
    NetHandle, NetPolicy, PolicyError, ProcPolicy, Redactor, SecretHandle, SecretResolver,
    SecretString, UserAnswer,
};

pub use net::{NetConfig, host_allowed};

use crate::prompt::{NoUserPrompter, UserPrompter};
use crate::secrets::SecretSource;

/// The native host. Construct with [`NativeHost::new`]; attach a prompter with
/// [`NativeHost::with_prompter`] and network settings with [`NativeHost::with_net_config`].
///
/// `Debug` prints secret *names* only.
pub struct NativeHost {
    redactor: Arc<Redactor>,
    secrets: Arc<dyn SecretSource>,
    prompter: Arc<dyn UserPrompter>,
    net: NetConfig,
    client: OnceLock<Result<reqwest::Client, String>>,
}

impl NativeHost {
    /// A host with no user attached ([`NoUserPrompter`]) and [`NetConfig::from_env`].
    pub fn new(redactor: Arc<Redactor>, secrets: Arc<dyn SecretSource>) -> NativeHost {
        NativeHost {
            redactor,
            secrets,
            prompter: Arc::new(NoUserPrompter),
            net: NetConfig::from_env(),
            client: OnceLock::new(),
        }
    }

    /// Route `ask_user` through `prompter`.
    pub fn with_prompter(mut self, prompter: Arc<dyn UserPrompter>) -> NativeHost {
        self.prompter = prompter;
        self
    }

    /// Replace the network settings (proxy, CA bundle). Resets any client built so far.
    pub fn with_net_config(mut self, net: NetConfig) -> NativeHost {
        self.net = net;
        self.client = OnceLock::new();
        self
    }

    /// The shared redactor.
    pub fn redactor(&self) -> &Arc<Redactor> {
        &self.redactor
    }

    /// The network settings in effect.
    pub fn net_config(&self) -> &NetConfig {
        &self.net
    }
}

impl fmt::Debug for NativeHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeHost")
            .field("secrets", &self.secrets)
            .field("prompter", &self.prompter)
            .field("net", &self.net)
            .field("redactor", &self.redactor)
            .finish()
    }
}

#[async_trait]
impl Host for NativeHost {
    fn name(&self) -> &str {
        "native"
    }

    async fn read_file(&self, policy: &FsPolicy, path: &Path) -> Result<Vec<u8>, HostError> {
        let r = fs::resolve(policy, path, FsMode::Ro).await?;
        tokio::fs::read(&r.resolved)
            .await
            .map_err(|e| fs::map_io(path, e))
    }

    async fn write_file(
        &self,
        policy: &FsPolicy,
        path: &Path,
        bytes: &[u8],
    ) -> Result<(), HostError> {
        let r = fs::resolve(policy, path, FsMode::Rw).await?;
        tokio::fs::write(&r.resolved, bytes)
            .await
            .map_err(|e| fs::map_io(path, e))
    }

    async fn list_dir(&self, policy: &FsPolicy, path: &Path) -> Result<Vec<DirEntry>, HostError> {
        let r = fs::resolve(policy, path, FsMode::Ro).await?;
        fs::list_dir(path, &r.resolved).await
    }

    async fn stat(&self, policy: &FsPolicy, path: &Path) -> Result<Metadata, HostError> {
        let r = fs::resolve(policy, path, FsMode::Ro).await?;
        fs::stat(path, &r.resolved).await
    }

    async fn remove(&self, policy: &FsPolicy, path: &Path) -> Result<(), HostError> {
        let r = fs::resolve(policy, path, FsMode::Rw).await?;
        fs::remove(path, &r.lexical).await
    }

    async fn spawn(
        &self,
        policy: &ProcPolicy,
        cmd: Command,
    ) -> Result<Box<dyn ChildProcess>, HostError> {
        process::spawn(policy, cmd).await
    }

    fn network(&self, policy: &NetPolicy) -> Result<Arc<dyn NetHandle>, HostError> {
        if !policy.enabled {
            return Err(HostError::Denied(PolicyError::NetDenied("*".to_owned())));
        }
        let client = self
            .client
            .get_or_init(|| net::build_client(&self.net))
            .as_ref()
            .map_err(|e| HostError::Net(e.clone()))?
            .clone();
        Ok(Arc::new(net::NativeNet::new(client, policy.allow.clone())))
    }

    fn secret(&self, name: &str) -> Result<SecretHandle, HostError> {
        if self.secrets.contains(name) {
            Ok(SecretHandle::with_locator(
                name,
                format!("{}:{name}", self.secrets.kind()),
            ))
        } else {
            Err(HostError::UnknownSecret(name.to_owned()))
        }
    }

    async fn ask_user(&self, req: AskUserRequest) -> Result<UserAnswer, HostError> {
        self.prompter.ask(req).await
    }
}

impl SecretResolver for NativeHost {
    /// Looks the handle's name up in the secret source and registers the value with the shared
    /// `Redactor` (as a named secret, so a too-short value is reported under its name) before
    /// returning it (§7.5). The locator is informational; a handle deserialized without one still
    /// resolves.
    fn resolve_secret(&self, handle: &SecretHandle) -> Result<SecretString, HostError> {
        let value = self
            .secrets
            .get(handle.name())
            .ok_or_else(|| HostError::UnknownSecret(handle.name().to_owned()))?;
        self.redactor.register_named_secret(handle.name(), &value);
        Ok(value)
    }
}
