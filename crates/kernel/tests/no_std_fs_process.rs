//! P1.6 rule: nothing in `kernel` touches `std::fs` or `std::process` directly, except the event
//! log writer (`log/file.rs`), which is the kernel's own durable store and takes no policy.

use std::path::Path;

fn walk(dir: &Path, out: &mut Vec<(String, String)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push((
                path.display().to_string(),
                std::fs::read_to_string(&path).unwrap(),
            ));
        }
    }
}

#[test]
fn kernel_sources_do_not_touch_std_fs_or_std_process() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    walk(&src, &mut files);
    assert!(files.len() > 10);
    for (path, text) in files {
        if path.ends_with("log/file.rs") {
            continue;
        }
        for needle in ["std::fs", "std::process", "tokio::fs", "tokio::process"] {
            assert!(
                !text.contains(needle),
                "{path} mentions `{needle}`; kernel code goes through Host / SandboxBackend"
            );
        }
    }
}
