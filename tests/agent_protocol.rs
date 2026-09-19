#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn adapter(input: Vec<u8>) -> Output {
    let runtime = PathBuf::from(format!(
        "/tmp/lr-agent-protocol-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_latchrun"))
        .arg("--runtime-dir")
        .arg(&runtime)
        .args(["agent", "serve"])
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    // Drain both result streams while writing requests, including an oversized one.
    let writer = thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    fs::remove_dir_all(runtime).unwrap();
    output
}

fn frame(request: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(request).unwrap();
    bytes.push(b'\n');
    bytes
}

fn responses(output: &Output) -> Vec<Value> {
    output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect()
}

#[test]
fn rejected_oversized_frame_cannot_process_a_valid_request_suffix() {
    let mut input = frame(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}));
    // The newline only appears AFTER the apparent ping: this is one invalid frame.
    input.extend(vec![b'x'; 1_048_577]);
    input.extend(frame(&json!({"jsonrpc":"2.0","id":2,"method":"ping"})));
    input.extend(frame(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"latchrun_stop","arguments":{"session":"not-authorized"}}})));
    let output = adapter(input);
    assert!(!output.status.success());
    let replies = responses(&output);
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["id"], 1);
    assert_eq!(replies[1]["error"]["code"], -32700);
    assert!(replies[1]["id"].is_null());
}

#[test]
fn malformed_json_closes_the_adapter_without_echoing_input() {
    let mut input = b"{sensitive-malformed-input}\n".to_vec();
    input.extend(frame(&json!({"jsonrpc":"2.0","id":2,"method":"ping"})));
    let output = adapter(input);
    assert!(!output.status.success());
    let replies = responses(&output);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["error"]["code"], -32700);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("sensitive-malformed-input"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("sensitive-malformed-input"));
}

#[test]
fn invalid_ids_are_rejected_without_echoing_embedded_data() {
    let mut input = Vec::new();
    for id in [
        json!({"private":"sensitive-invalid-id"}),
        json!(["sensitive-invalid-id"]),
        json!(true),
        Value::Null,
        json!(1.5),
    ] {
        input.extend(frame(&json!({"jsonrpc":"2.0","id":id,"method":"ping"})));
    }
    input.extend(frame(&json!(["sensitive-invalid-envelope"])));
    input.extend(frame(
        &json!({"jsonrpc":"2.0","id":"valid-id","method":"ping"}),
    ));
    input.extend(frame(&json!({"jsonrpc":"2.0","id":17,"method":"ping"})));
    let output = adapter(input);
    assert!(output.status.success());
    let replies = responses(&output);
    assert_eq!(replies.len(), 8);
    for reply in &replies[..6] {
        assert_eq!(reply["error"]["code"], -32600);
        assert!(reply["id"].is_null());
    }
    assert_eq!(replies[6]["id"], "valid-id");
    assert_eq!(replies[7]["id"], 17);
    assert_eq!(replies[7]["result"], json!({}));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("sensitive-invalid"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("sensitive-invalid"));
}
