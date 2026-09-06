//! Path resolution and policy checks for the native filesystem calls (D5).
//!
//! Every call: the path must be absolute; it is normalized lexically (`.` dropped, `..` refused);
//! the *lexical* path is checked against the policy; then symlinks are resolved for the existing
//! prefix (the deepest existing ancestor is canonicalized and the rest re-appended; a dangling
//! symlink is followed by hand so a write cannot land at its target unchecked) and the *resolved*
//! path is checked again against the policy with its mount paths resolved the same way. Both
//! checks must pass: a symlink inside a mount that points outside is denied, and a path outside
//! every mount is denied even if it links into one (bwrap would not show it at all).

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};

use kernel::{DirEntry, FsMode, FsPolicy, HostError, Metadata, Mount, PolicyError};

/// Cap on dangling-symlink hops followed by hand (the OS caps the rest with `ELOOP`).
const MAX_SYMLINK_HOPS: u32 = 40;

/// The two views of a checked path.
pub(crate) struct Resolved {
    /// Absolute, normalized, symlinks not followed. Used by `remove` so a link, not its target,
    /// is removed.
    pub lexical: PathBuf,
    /// Symlinks of the existing prefix resolved. Used by every other operation.
    pub resolved: PathBuf,
}

/// Map an I/O error: not-found → `NotFound`, anything else → `Io`.
pub(crate) fn map_io(path: &Path, e: io::Error) -> HostError {
    if e.kind() == io::ErrorKind::NotFound {
        HostError::NotFound(path.display().to_string())
    } else {
        HostError::Io(format!("{}: {e}", path.display()))
    }
}

/// Lexical normalization. `None` when the path contains `..`.
fn normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::RootDir | Component::Prefix(_) => out.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => return None,
            Component::Normal(n) => out.push(n),
        }
    }
    Some(out)
}

/// Canonicalize the deepest existing ancestor of `path` and re-append the rest.
pub(crate) async fn resolve_symlinks(path: &Path) -> Result<PathBuf, HostError> {
    let mut cur = path.to_path_buf();
    let mut hops = 0u32;
    'follow: loop {
        let mut probe = cur.clone();
        let mut rest: Vec<OsString> = Vec::new();
        loop {
            match tokio::fs::canonicalize(&probe).await {
                Ok(mut canon) => {
                    for r in rest.iter().rev() {
                        canon.push(r);
                    }
                    return normalize(&canon).ok_or_else(|| {
                        HostError::Io(format!(
                            "{}: unresolvable `..` in a symlink target",
                            path.display()
                        ))
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    // A dangling symlink: follow it by hand.
                    if let Ok(meta) = tokio::fs::symlink_metadata(&probe).await
                        && meta.file_type().is_symlink()
                    {
                        hops += 1;
                        if hops > MAX_SYMLINK_HOPS {
                            return Err(HostError::Io(format!(
                                "{}: too many levels of symbolic links",
                                path.display()
                            )));
                        }
                        let target = tokio::fs::read_link(&probe)
                            .await
                            .map_err(|e| map_io(&probe, e))?;
                        let parent = probe.parent().unwrap_or_else(|| Path::new("/"));
                        let mut next = if target.is_absolute() {
                            target
                        } else {
                            parent.join(target)
                        };
                        for r in rest.iter().rev() {
                            next.push(r);
                        }
                        cur = next;
                        continue 'follow;
                    }
                    match (
                        probe.file_name().map(ToOwned::to_owned),
                        probe.parent().map(Path::to_path_buf),
                    ) {
                        (Some(name), Some(parent)) => {
                            rest.push(name);
                            probe = parent;
                        }
                        _ => return Err(map_io(path, e)),
                    }
                }
                Err(e) => return Err(map_io(path, e)),
            }
        }
    }
}

/// Resolve `path` and check it against `policy` for `mode`.
pub(crate) async fn resolve(
    policy: &FsPolicy,
    path: &Path,
    mode: FsMode,
) -> Result<Resolved, HostError> {
    let denied = || {
        HostError::Denied(PolicyError::PathDenied {
            path: path.display().to_string(),
            mode,
        })
    };
    if !path.is_absolute() {
        return Err(denied());
    }
    let lexical = normalize(path).ok_or_else(denied)?;
    policy.check(&lexical, mode).map_err(|_| denied())?;

    let resolved = resolve_symlinks(&lexical).await?;
    let mut mounts = Vec::with_capacity(policy.mounts.len());
    for m in &policy.mounts {
        mounts.push(Mount {
            path: resolve_symlinks(&m.path).await?,
            mode: m.mode,
        });
    }
    FsPolicy { mounts }
        .check(&resolved, mode)
        .map_err(|_| denied())?;
    Ok(Resolved { lexical, resolved })
}

fn modified_rfc3339(meta: &std::fs::Metadata) -> Option<String> {
    let t = meta.modified().ok()?;
    let d = t.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(kernel::time::format_rfc3339_ms(d.as_millis()))
}

pub(crate) async fn stat(orig: &Path, resolved: &Path) -> Result<Metadata, HostError> {
    let meta = tokio::fs::metadata(resolved)
        .await
        .map_err(|e| map_io(orig, e))?;
    Ok(Metadata {
        is_dir: meta.is_dir(),
        size: meta.len(),
        modified: modified_rfc3339(&meta),
    })
}

pub(crate) async fn list_dir(orig: &Path, resolved: &Path) -> Result<Vec<DirEntry>, HostError> {
    let mut rd = tokio::fs::read_dir(resolved)
        .await
        .map_err(|e| map_io(orig, e))?;
    let mut out = Vec::new();
    while let Some(entry) = rd.next_entry().await.map_err(|e| map_io(orig, e))? {
        // Follows symlinks like `ls -L`; a dangling link reports as a zero-size file.
        let (is_dir, size) = match tokio::fs::metadata(entry.path()).await {
            Ok(m) => (m.is_dir(), m.len()),
            Err(_) => (false, 0),
        };
        out.push(DirEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir,
            size,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Remove a file, symlink, or empty directory at the *lexical* path.
pub(crate) async fn remove(orig: &Path, lexical: &Path) -> Result<(), HostError> {
    let meta = tokio::fs::symlink_metadata(lexical)
        .await
        .map_err(|e| map_io(orig, e))?;
    if meta.is_dir() {
        tokio::fs::remove_dir(lexical).await
    } else {
        tokio::fs::remove_file(lexical).await
    }
    .map_err(|e| map_io(orig, e))
}
