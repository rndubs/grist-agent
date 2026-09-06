//! Filesystem policy enforcement (D5): every native fs call checks the path against the
//! `FsPolicy` after normalizing it and resolving symlinks.

mod common;

use std::path::{Path, PathBuf};

use common::{fs_policy, host, mount};
use kernel::{FsMode, Host, HostError, PolicyError};
use tempfile::TempDir;

struct Fx {
    _tmp: TempDir,
    /// Canonical temp root.
    root: PathBuf,
}

fn fixture() -> Fx {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonical");
    std::fs::create_dir_all(root.join("ro")).unwrap();
    std::fs::create_dir_all(root.join("rw/nested")).unwrap();
    std::fs::create_dir_all(root.join("outside")).unwrap();
    std::fs::write(root.join("ro/a.txt"), b"alpha").unwrap();
    std::fs::write(root.join("rw/b.txt"), b"beta").unwrap();
    std::fs::write(root.join("rw/nested/c.txt"), b"gamma").unwrap();
    std::fs::write(root.join("outside/secret.txt"), b"nope").unwrap();
    Fx { _tmp: tmp, root }
}

fn assert_denied(r: Result<impl std::fmt::Debug, HostError>, mode: FsMode) {
    match r {
        Err(HostError::Denied(PolicyError::PathDenied { mode: m, .. })) => assert_eq!(m, mode),
        other => panic!("expected PathDenied({mode:?}), got {other:?}"),
    }
}

#[tokio::test]
async fn read_inside_ro_mount_is_allowed() {
    let fx = fixture();
    let policy = fs_policy(vec![mount(&fx.root.join("ro"), FsMode::Ro)]);
    let bytes = host()
        .read_file(&policy, &fx.root.join("ro/a.txt"))
        .await
        .unwrap();
    assert_eq!(bytes, b"alpha");
}

#[tokio::test]
async fn write_into_ro_mount_is_denied() {
    let fx = fixture();
    let policy = fs_policy(vec![mount(&fx.root.join("ro"), FsMode::Ro)]);
    let r = host()
        .write_file(&policy, &fx.root.join("ro/new.txt"), b"x")
        .await;
    assert_denied(r, FsMode::Rw);
    assert!(!fx.root.join("ro/new.txt").exists());
}

#[tokio::test]
async fn write_into_rw_mount_creates_and_overwrites() {
    let fx = fixture();
    let policy = fs_policy(vec![mount(&fx.root.join("rw"), FsMode::Rw)]);
    let h = host();
    let p = fx.root.join("rw/new.txt");
    h.write_file(&policy, &p, b"one").await.unwrap();
    h.write_file(&policy, &p, b"two").await.unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"two");
}

#[tokio::test]
async fn write_with_missing_parent_is_not_found() {
    let fx = fixture();
    let policy = fs_policy(vec![mount(&fx.root.join("rw"), FsMode::Rw)]);
    let r = host()
        .write_file(&policy, &fx.root.join("rw/missing/dir/new.txt"), b"x")
        .await;
    assert!(matches!(r, Err(HostError::NotFound(_))), "{r:?}");
}

#[tokio::test]
async fn read_outside_every_mount_is_denied() {
    let fx = fixture();
    let policy = fs_policy(vec![
        mount(&fx.root.join("ro"), FsMode::Ro),
        mount(&fx.root.join("rw"), FsMode::Rw),
    ]);
    let r = host()
        .read_file(&policy, &fx.root.join("outside/secret.txt"))
        .await;
    assert_denied(r, FsMode::Ro);
    let r = host().read_file(&policy, Path::new("/etc/hostname")).await;
    assert_denied(r, FsMode::Ro);
}

#[tokio::test]
async fn read_with_no_mounts_is_denied() {
    let fx = fixture();
    let r = host()
        .read_file(&fs_policy(vec![]), &fx.root.join("ro/a.txt"))
        .await;
    assert_denied(r, FsMode::Ro);
}

#[tokio::test]
async fn symlink_inside_mount_pointing_outside_is_denied_for_read_and_write() {
    let fx = fixture();
    let link = fx.root.join("rw/escape.txt");
    std::os::unix::fs::symlink(fx.root.join("outside/secret.txt"), &link).unwrap();
    let policy = fs_policy(vec![mount(&fx.root.join("rw"), FsMode::Rw)]);
    let h = host();
    assert_denied(h.read_file(&policy, &link).await, FsMode::Ro);
    assert_denied(h.write_file(&policy, &link, b"pwned").await, FsMode::Rw);
    assert_eq!(
        std::fs::read(fx.root.join("outside/secret.txt")).unwrap(),
        b"nope"
    );
}

#[tokio::test]
async fn symlinked_directory_pointing_outside_is_denied() {
    let fx = fixture();
    std::os::unix::fs::symlink(fx.root.join("outside"), fx.root.join("rw/out")).unwrap();
    let policy = fs_policy(vec![mount(&fx.root.join("rw"), FsMode::Rw)]);
    let h = host();
    assert_denied(
        h.read_file(&policy, &fx.root.join("rw/out/secret.txt"))
            .await,
        FsMode::Ro,
    );
    assert_denied(
        h.write_file(&policy, &fx.root.join("rw/out/new.txt"), b"x")
            .await,
        FsMode::Rw,
    );
    assert_denied(
        h.list_dir(&policy, &fx.root.join("rw/out")).await,
        FsMode::Ro,
    );
    assert!(!fx.root.join("outside/new.txt").exists());
}

#[tokio::test]
async fn dangling_symlink_pointing_outside_cannot_be_written_through() {
    let fx = fixture();
    let link = fx.root.join("rw/dangling.txt");
    std::os::unix::fs::symlink(fx.root.join("outside/created.txt"), &link).unwrap();
    let policy = fs_policy(vec![mount(&fx.root.join("rw"), FsMode::Rw)]);
    assert_denied(host().write_file(&policy, &link, b"x").await, FsMode::Rw);
    assert!(!fx.root.join("outside/created.txt").exists());
}

#[tokio::test]
async fn symlink_inside_mount_pointing_inside_is_allowed() {
    let fx = fixture();
    let link = fx.root.join("rw/alias.txt");
    std::os::unix::fs::symlink(fx.root.join("rw/b.txt"), &link).unwrap();
    let policy = fs_policy(vec![mount(&fx.root.join("rw"), FsMode::Rw)]);
    let h = host();
    assert_eq!(h.read_file(&policy, &link).await.unwrap(), b"beta");
    h.write_file(&policy, &link, b"BETA").await.unwrap();
    assert_eq!(std::fs::read(fx.root.join("rw/b.txt")).unwrap(), b"BETA");
}

#[tokio::test]
async fn path_outside_mount_linking_into_it_is_denied() {
    let fx = fixture();
    std::os::unix::fs::symlink(fx.root.join("rw"), fx.root.join("outside/into_rw")).unwrap();
    let policy = fs_policy(vec![mount(&fx.root.join("rw"), FsMode::Rw)]);
    assert_denied(
        host()
            .read_file(&policy, &fx.root.join("outside/into_rw/b.txt"))
            .await,
        FsMode::Ro,
    );
}

#[tokio::test]
async fn dot_dot_is_denied_even_when_it_would_land_inside() {
    let fx = fixture();
    let policy = fs_policy(vec![mount(&fx.root.join("rw"), FsMode::Rw)]);
    let h = host();
    assert_denied(
        h.read_file(&policy, &fx.root.join("rw/nested/../b.txt"))
            .await,
        FsMode::Ro,
    );
    assert_denied(
        h.read_file(&policy, &fx.root.join("rw/../outside/secret.txt"))
            .await,
        FsMode::Ro,
    );
}

#[tokio::test]
async fn relative_path_is_denied() {
    let fx = fixture();
    let policy = fs_policy(vec![mount(&fx.root.join("rw"), FsMode::Rw)]);
    assert_denied(
        host().read_file(&policy, Path::new("rw/b.txt")).await,
        FsMode::Ro,
    );
}

#[tokio::test]
async fn nested_ro_mount_under_rw_mount_denies_writes_but_allows_reads() {
    let fx = fixture();
    let policy = fs_policy(vec![
        mount(&fx.root.join("rw"), FsMode::Rw),
        mount(&fx.root.join("rw/nested"), FsMode::Ro),
    ]);
    let h = host();
    assert_denied(
        h.write_file(&policy, &fx.root.join("rw/nested/c.txt"), b"x")
            .await,
        FsMode::Rw,
    );
    assert_denied(
        h.remove(&policy, &fx.root.join("rw/nested/c.txt")).await,
        FsMode::Rw,
    );
    assert_eq!(
        h.read_file(&policy, &fx.root.join("rw/nested/c.txt"))
            .await
            .unwrap(),
        b"gamma"
    );
    h.write_file(&policy, &fx.root.join("rw/ok.txt"), b"x")
        .await
        .unwrap();
}

#[tokio::test]
async fn list_dir_respects_policy_and_lists_entries() {
    let fx = fixture();
    let policy = fs_policy(vec![mount(&fx.root.join("rw"), FsMode::Ro)]);
    let h = host();
    let entries = h.list_dir(&policy, &fx.root.join("rw")).await.unwrap();
    let names: Vec<(&str, bool)> = entries
        .iter()
        .map(|e| (e.name.as_str(), e.is_dir))
        .collect();
    assert_eq!(names, vec![("b.txt", false), ("nested", true)]);
    assert_eq!(entries[0].size, 4);
    assert_denied(
        h.list_dir(&policy, &fx.root.join("outside")).await,
        FsMode::Ro,
    );
    assert_denied(h.list_dir(&policy, &fx.root).await, FsMode::Ro);
}

#[tokio::test]
async fn stat_respects_policy_and_reports_metadata() {
    let fx = fixture();
    let policy = fs_policy(vec![mount(&fx.root.join("ro"), FsMode::Ro)]);
    let h = host();
    let m = h.stat(&policy, &fx.root.join("ro/a.txt")).await.unwrap();
    assert!(!m.is_dir);
    assert_eq!(m.size, 5);
    let modified = m.modified.expect("modified");
    assert!(
        modified.ends_with('Z') && modified.contains('T'),
        "{modified}"
    );
    let d = h.stat(&policy, &fx.root.join("ro")).await.unwrap();
    assert!(d.is_dir);
    assert!(matches!(
        h.stat(&policy, &fx.root.join("ro/none.txt")).await,
        Err(HostError::NotFound(_))
    ));
    assert_denied(
        h.stat(&policy, &fx.root.join("outside/secret.txt")).await,
        FsMode::Ro,
    );
}

#[tokio::test]
async fn remove_respects_policy_and_removes_links_not_targets() {
    let fx = fixture();
    let policy = fs_policy(vec![
        mount(&fx.root.join("ro"), FsMode::Ro),
        mount(&fx.root.join("rw"), FsMode::Rw),
    ]);
    let h = host();
    assert_denied(
        h.remove(&policy, &fx.root.join("ro/a.txt")).await,
        FsMode::Rw,
    );
    assert!(fx.root.join("ro/a.txt").exists());

    h.remove(&policy, &fx.root.join("rw/b.txt")).await.unwrap();
    assert!(!fx.root.join("rw/b.txt").exists());
    assert!(matches!(
        h.remove(&policy, &fx.root.join("rw/b.txt")).await,
        Err(HostError::NotFound(_))
    ));

    // An empty directory is removed; a non-empty one is an I/O error.
    std::fs::create_dir(fx.root.join("rw/empty")).unwrap();
    h.remove(&policy, &fx.root.join("rw/empty")).await.unwrap();
    assert!(!fx.root.join("rw/empty").exists());
    assert!(matches!(
        h.remove(&policy, &fx.root.join("rw/nested")).await,
        Err(HostError::Io(_))
    ));

    // A link inside the mount to a file inside the mount: the link goes, the target stays.
    let link = fx.root.join("rw/link.txt");
    std::os::unix::fs::symlink(fx.root.join("rw/nested/c.txt"), &link).unwrap();
    h.remove(&policy, &link).await.unwrap();
    assert!(!link.exists());
    assert!(fx.root.join("rw/nested/c.txt").exists());

    // A link to an Ro target is denied (the target's mode decides).
    let link = fx.root.join("rw/to_ro.txt");
    std::os::unix::fs::symlink(fx.root.join("ro/a.txt"), &link).unwrap();
    assert_denied(h.remove(&policy, &link).await, FsMode::Rw);
}

#[tokio::test]
async fn read_of_missing_file_inside_mount_is_not_found() {
    let fx = fixture();
    let policy = fs_policy(vec![mount(&fx.root.join("ro"), FsMode::Ro)]);
    let r = host()
        .read_file(&policy, &fx.root.join("ro/none.txt"))
        .await;
    assert!(matches!(r, Err(HostError::NotFound(_))), "{r:?}");
}
