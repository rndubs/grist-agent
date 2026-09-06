//! `diff-logs <recorded.jsonl> <replayed.jsonl>` (D16, `event-schema.md` §5.3): compares the two
//! logs' effective `(kind, payload)` sequences after stripping the volatile fields. Exit status 0
//! when identical, 1 otherwise (the first divergence is printed; usage and read errors are
//! reported on stderr).

use std::fmt;
use std::path::Path;

use kernel::log::FileEventLog;
use kernel::replay::diff_logs;

/// A failure whose `Debug` is its plain message, so `main`'s `Err` prints `Error: <message>`.
struct Failure(String);

impl fmt::Debug for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<E: fmt::Display> From<E> for Failure {
    fn from(e: E) -> Self {
        Failure(e.to_string())
    }
}

fn main() -> Result<(), Failure> {
    let args: Vec<String> = std::env::args().collect();
    let [_, recorded, replayed] = args.as_slice() else {
        return Err(Failure(
            "usage: diff-logs <recorded.jsonl> <replayed.jsonl>".to_owned(),
        ));
    };
    let a = FileEventLog::snapshot(Path::new(recorded))?;
    let b = FileEventLog::snapshot(Path::new(replayed))?;
    let report = diff_logs(&a, &b)?;
    match report.first_diff {
        None => {
            println!("identical: {} events compared", report.compared);
            Ok(())
        }
        Some((index, description)) => {
            println!("divergence at index {index}: {description}");
            Err(Failure("logs differ".to_owned()))
        }
    }
}
