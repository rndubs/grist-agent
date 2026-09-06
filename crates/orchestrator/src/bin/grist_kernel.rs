//! `grist-kernel`: one session over stdio (D4). This is the process `grist-daemon` spawns per
//! session, and it is also a complete ACP agent on its own: an ACP client can launch it directly
//! for a local, single-session setup with no daemon.
//!
//! Environment: `GRIST_STATE_DIR` (default `~/.grist`), `GRIST_PROFILES_DIR` (default: the
//! `profiles/` tree found upward from the current directory), `GRIST_AGENT` (catalog entry,
//! default `default`), `GRIST_SANDBOX_BACKEND` (development override, D14), and
//! `GRIST_ENDPOINT_<NAME>_URL` for the model profile's endpoint.

use agent_client_protocol::{ByteStreams, ConnectTo};
use orchestrator::acp::{Server, ServerOptions, agent_component};
use orchestrator::launcher::LaunchOptions;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let launch = LaunchOptions::from_env()?;
    let default_agent = std::env::var("GRIST_AGENT").unwrap_or_else(|_| "default".to_owned());
    eprintln!(
        "grist-kernel: profiles {} · state {} · agent {}",
        launch.profiles_dir.display(),
        launch.state_dir.display(),
        default_agent
    );
    let server = Server::new(ServerOptions {
        launch,
        single_session: true,
        default_agent,
        provider_override: None,
    });
    let bytes = ByteStreams::new(
        tokio::io::stdout().compat_write(),
        tokio::io::stdin().compat(),
    );
    agent_component(server).connect_to(bytes).await?;
    Ok(())
}
