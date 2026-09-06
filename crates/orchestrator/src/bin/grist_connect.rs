//! `grist-connect`: the stdio ↔ daemon forwarder an ACP client spawns (ADR-0004). Usage:
//!
//! ```text
//! grist-connect [--socket PATH | --tcp HOST:PORT]
//! ```
//!
//! Default: the unix socket `<GRIST_STATE_DIR>/daemon.sock` (`~/.grist/daemon.sock`). For a
//! remote daemon, forward its socket with ssh first (D11), e.g.
//! `ssh -N -L /tmp/grist.sock:/home/me/.grist/daemon.sock login-node` and point `--socket` at
//! it, or `ssh -N -L 7411:/home/me/.grist/daemon.sock login-node` and use `--tcp 127.0.0.1:7411`
//! (the only option on Windows, which has no unix sockets).

use std::path::PathBuf;

use orchestrator::acp::connect::{Target, forward};

fn usage() -> ! {
    eprintln!("usage: grist-connect [--socket PATH | --tcp HOST:PORT]");
    std::process::exit(2)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let state_dir = std::env::var_os("GRIST_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".grist")))
        .unwrap_or_else(|| PathBuf::from(".grist"));
    let mut target = Target::Unix(state_dir.join("daemon.sock"));
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => {
                target = Target::Unix(PathBuf::from(args.next().unwrap_or_else(|| usage())))
            }
            "--tcp" => target = Target::Tcp(args.next().unwrap_or_else(|| usage())),
            "-h" | "--help" => usage(),
            _ => usage(),
        }
    }
    forward(&target, tokio::io::stdin(), tokio::io::stdout()).await?;
    Ok(())
}
