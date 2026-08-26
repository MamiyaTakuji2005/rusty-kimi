//! A momentary lock on context.jsonl must not kill the turn.
//!
//! On Windows a scanner, an indexer, or the `Fork` tool's own `fs::copy` of
//! the parent context can hold the file for a few milliseconds. That surfaced
//! as `os error 32` out of `update_token_count`, which aborts the whole prompt.

use kimi_agent::soul::context::Context;
use tempfile::TempDir;

#[cfg(windows)]
#[tokio::test]
async fn test_append_waits_out_a_transient_lock() {
    use std::os::windows::fs::OpenOptionsExt;

    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("context.jsonl");
    std::fs::write(&path, "").expect("seed context");

    // Hold the file with no sharing at all, exactly what makes another
    // process's append fail with ERROR_SHARING_VIOLATION. Wait for the lock
    // to be in place before appending, so the retry path is really taken.
    let locked_path = path.clone();
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&locked_path)
            .expect("exclusive open");
        locked_tx.send(()).expect("signal lock held");
        std::thread::sleep(std::time::Duration::from_millis(150));
        drop(file);
    });
    locked_rx.recv().expect("lock held");

    let mut context = Context::new(path.clone());
    context
        .update_token_count(42)
        .await
        .expect("append should survive a transient lock");

    holder.join().expect("holder thread");
    let contents = std::fs::read_to_string(&path).expect("read context");
    assert!(contents.contains("\"token_count\":42"));
}

#[tokio::test]
async fn test_append_error_names_the_file() {
    let dir = TempDir::new().expect("temp dir");
    // A directory where the context file should be: the open fails for a
    // reason no retry can fix, so the error must identify what it was.
    let path = dir.path().join("context.jsonl");
    std::fs::create_dir(&path).expect("directory in the way");

    let mut context = Context::new(path.clone());
    let err = context
        .update_token_count(1)
        .await
        .expect_err("append to a directory must fail");

    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("context.jsonl"),
        "error should name the file: {rendered}"
    );
}
