mod common;

use common::*;
use serde_json::json;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use teams_cli::daemon::{self, SyncConfig, SyncRunner};

#[test]
fn executor_app_schemas_validate_against_daemon() {
    let executor_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("executor-app");
    if !executor_dir.join("node_modules/apps").is_dir() {
        eprintln!("skipping executor_schema test: executor-app/node_modules/apps not installed");
        return;
    }
    if Command::new("bun").arg("--version").output().is_err() {
        eprintln!("skipping executor_schema test: bun not on PATH");
        return;
    }

    let _g = env_lock();
    let (_td, _home) = home_with_archive();
    let runner: SyncRunner = Arc::new(|_| Ok(json!({"messages_new": 0})));
    let handle = daemon::start(0, Some(SyncConfig { runner, interval: None }), false).unwrap();
    let url = format!("http://{}", handle.addr());

    let out = Command::new("bun")
        .args(["test", "executor_schema.test.ts"])
        .current_dir(&executor_dir)
        .env("TEAMS_URL", &url)
        .env("TEAMS_TOKEN", api_token())
        .output()
        .unwrap();

    handle.shutdown();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains(" skip") && stderr.contains("1 pass"),
        "executor schema test did not run:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        out.status.success(),
        "bun test executor_schema.test.ts failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}
