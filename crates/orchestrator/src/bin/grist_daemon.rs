//! `grist-daemon`: the unix-socket supervisor (D4, D11). Usage:
//!
//! ```text
//! grist-daemon [--socket PATH] [--kernel-bin PATH]
//! ```
//!
//! Defaults: socket `<GRIST_STATE_DIR>/daemon.sock` (`~/.grist/daemon.sock`), kernel binary
//! `grist-kernel` next to this executable (or `GRIST_KERNEL_BIN`). The launcher environment
//! (`GRIST_PROFILES_DIR`, `GRIST_SANDBOX_BACKEND`, `GRIST_ENDPOINT_*`) is passed to every
//! kernel process.

use std::path::PathBuf;
use std::sync::Arc;

use orchestrator::acp::daemon::{DaemonOptions, bind, serve};
use orchestrator::launcher::LaunchOptions;

fn usage() -> ! {
    eprintln!("usage: grist-daemon [--socket PATH] [--kernel-bin PATH]");
    std::process::exit(2)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let launch = LaunchOptions::from_env()?;
    let mut socket = launch.state_dir.join("daemon.sock");
    let mut kernel_bin = std::env::var_os("GRIST_KERNEL_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .map(|p| p.with_file_name("grist-kernel"))
                .unwrap_or_else(|_| PathBuf::from("grist-kernel"))
        });
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => socket = PathBuf::from(args.next().unwrap_or_else(|| usage())),
            "--kernel-bin" => kernel_bin = PathBuf::from(args.next().unwrap_or_else(|| usage())),
            "-h" | "--help" => usage(),
            _ => usage(),
        }
    }
    let listener = bind(&socket).await?;
    eprintln!(
        "grist-daemon: listening on {} · kernel {} · profiles {} · state {}",
        socket.display(),
        kernel_bin.display(),
        launch.profiles_dir.display(),
        launch.state_dir.display()
    );
    serve(
        listener,
        Arc::new(DaemonOptions {
            kernel_bin,
            launch,
            env: Vec::new(),
        }),
    )
    .await?;
    Ok(())
}
