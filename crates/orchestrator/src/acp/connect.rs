//! `grist-connect`: an off-the-shelf ACP client can only spawn a subprocess and talk newline
//! JSON-RPC over its stdio (ADR-0004). This forwarder is that subprocess: it connects to the
//! daemon's unix socket (D11) and pumps bytes both ways, unchanged. Remote use is the same binary
//! pointed at an SSH-forwarded socket; on a client platform without unix sockets (Windows, D14)
//! ssh forwards the daemon's socket to a local TCP port and [`Target::Tcp`] is used instead.

use std::path::PathBuf;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

/// Where the daemon is reachable from this process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A unix socket path (the daemon's own, or an ssh-forwarded copy of it).
    Unix(PathBuf),
    /// `host:port` of an ssh `-L` forward of the daemon's socket.
    Tcp(String),
}

/// Copy `input` → daemon and daemon → `output` until either side closes. Returns the byte
/// counts `(to_daemon, from_daemon)`; the second is `0` when the client closed first.
pub async fn forward<I, O>(target: &Target, input: I, output: O) -> std::io::Result<(u64, u64)>
where
    I: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
{
    match target {
        #[cfg(unix)]
        Target::Unix(path) => {
            let stream = tokio::net::UnixStream::connect(path).await?;
            let (rd, wr) = stream.into_split();
            pump(rd, wr, input, output).await
        }
        #[cfg(not(unix))]
        Target::Unix(path) => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!(
                "unix sockets are unavailable here; forward {} to a TCP port with ssh -L and use --tcp",
                path.display()
            ),
        )),
        Target::Tcp(addr) => {
            let stream = tokio::net::TcpStream::connect(addr).await?;
            let (rd, wr) = stream.into_split();
            pump(rd, wr, input, output).await
        }
    }
}

async fn pump<R, W, I, O>(
    mut rd: R,
    mut wr: W,
    mut input: I,
    mut output: O,
) -> std::io::Result<(u64, u64)>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    I: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
{
    let up = async {
        let n = tokio::io::copy(&mut input, &mut wr).await?;
        let _ = wr.shutdown().await;
        Ok::<u64, std::io::Error>(n)
    };
    let down = async {
        let n = tokio::io::copy(&mut rd, &mut output).await?;
        let _ = output.flush().await;
        Ok::<u64, std::io::Error>(n)
    };
    // Either direction ending ends the forwarder: a closed client stdin means "quit", and a
    // closed socket means the daemon is gone.
    tokio::select! {
        r = up => Ok((r?, 0)),
        r = down => Ok((0, r?)),
    }
}
