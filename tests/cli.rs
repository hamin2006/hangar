//! End-to-end tests that run the real `hangar` binary against temporary folders.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

/// Runs hangar with an isolated config and state folder so the user's setup can't leak in.
fn hangar(args: &[&str], state: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hangar"))
        .args(args)
        .env("HANGAR_CONFIG", state.join("no-config.toml"))
        .env("HANGAR_STATE_DIR", state)
        .env_remove("VERCEL_TOKEN")
        .output()
        .expect("run hangar")
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .expect("git")
        .success();
    assert!(ok, "git {args:?} failed");
}

#[test]
fn projects_json_reports_git_state() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let app = root.path().join("my-app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("package.json"),
        r#"{"scripts":{"dev":"vite"},"devDependencies":{"vite":"8"}}"#,
    )
    .unwrap();
    git(&app, &["init", "-q", "-b", "main"]);
    git(&app, &["add", "."]);
    git(&app, &["commit", "-q", "-m", "first commit"]);
    std::fs::write(app.join("new.txt"), "x").unwrap();
    // A plain folder with no markers is ignored; an empty repo is still listed.
    std::fs::create_dir_all(root.path().join("not-a-project")).unwrap();
    let empty = root.path().join("empty-repo");
    std::fs::create_dir_all(&empty).unwrap();
    git(&empty, &["init", "-q"]);

    let out = hangar(
        &[
            "projects",
            "--json",
            "--root",
            root.path().to_str().unwrap(),
        ],
        state.path(),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let list: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(list.len(), 2, "{list:#?}");

    let app = list.iter().find(|p| p["name"] == "my-app").unwrap();
    assert_eq!(app["kind"], "vite");
    assert_eq!(app["dev_command"], serde_json::json!(["npm", "run", "dev"]));
    assert_eq!(app["git"]["branch"], "main");
    assert_eq!(app["git"]["untracked"], 1);
    assert_eq!(app["git"]["commits"][0]["subject"], "first commit");

    let empty = list.iter().find(|p| p["name"] == "empty-repo").unwrap();
    assert!(empty["git"]["commits"].as_array().unwrap().is_empty());
    assert!(empty["git_error"].is_null());

    // Plain-text output works too, and a missing root is not an error.
    let out = hangar(
        &[
            "projects",
            "--root",
            root.path().join("missing").to_str().unwrap(),
        ],
        state.path(),
    );
    assert!(out.status.success());
}

#[test]
fn ports_lists_our_own_listener() {
    let state = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let out = hangar(
        &[
            "ports",
            "--all",
            "--json",
            "--root",
            state.path().to_str().unwrap(),
        ],
        state.path(),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let list: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
    let mine = list
        .iter()
        .find(|l| l["port"] == port)
        .unwrap_or_else(|| panic!("port {port} not found in {list:#?}"));
    assert_eq!(mine["pid"], std::process::id());
    assert_eq!(mine["addrs"], serde_json::json!(["127.0.0.1"]));
    drop(listener);
}

#[test]
fn kill_reports_empty_ports_and_refuses_itself() {
    let state = tempfile::tempdir().unwrap();
    // Find a port nobody is using.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let out = hangar(&["kill", &port.to_string()], state.path());
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("nothing is listening"));

    // Kill a real listener owned by a child process (never the test process itself).
    let mut child = Command::new("python3")
        .args(["-c", "import socket,time; s=socket.socket(); s.bind(('127.0.0.1',0)); s.listen(); print(s.getsockname()[1], flush=True); time.sleep(60)"])
        .stdout(std::process::Stdio::piped())
        .spawn();
    let Ok(child) = child.as_mut() else { return }; // python3 not available: skip this part
    let mut line = String::new();
    use std::io::BufRead;
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let child_port = line.trim().to_string();
    let out = hangar(&["kill", &child_port], state.path());
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("stopped"));
    let status = child.wait().unwrap();
    assert!(!status.success(), "child was terminated by a signal");
}

#[cfg(unix)]
#[test]
fn start_logs_stop_lifecycle() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let app = root.path().join("svc");
    std::fs::create_dir_all(&app).unwrap();
    // A package.json whose dev script prints a marker then idles, started through npm like a real app.
    if Command::new("npm").arg("--version").output().is_err() {
        eprintln!("npm not installed; skipping");
        return;
    }
    std::fs::write(
        app.join("package.json"),
        r#"{"name":"svc","scripts":{"dev":"echo svc-booted && sleep 60"}}"#,
    )
    .unwrap();
    let r = root.path().to_str().unwrap();

    let out = hangar(&["start", "svc", "--root", r], state.path());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = hangar(&["start", "svc", "--root", r], state.path());
    assert!(!out.status.success(), "second start is refused");
    assert!(String::from_utf8_lossy(&out.stderr).contains("already running"));

    std::thread::sleep(std::time::Duration::from_secs(1));
    let out = hangar(&["logs", "svc", "-n", "20"], state.path());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("svc-booted"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    let out = hangar(&["stop", "svc"], state.path());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = hangar(&["stop", "svc"], state.path());
    assert!(!out.status.success());

    let out = hangar(&["start", "nope", "--root", r], state.path());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no project named"));
}

#[test]
fn dashboard_refuses_without_a_terminal() {
    let state = tempfile::tempdir().unwrap();
    let out = hangar(&[], state.path());
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("interactive terminal"));
}

#[test]
fn bad_config_warns_but_runs() {
    let state = tempfile::tempdir().unwrap();
    let cfg = state.path().join("config.toml");
    std::fs::write(&cfg, "roots = 42").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_hangar"))
        .args(["projects", "--root", state.path().to_str().unwrap()])
        .env("HANGAR_CONFIG", &cfg)
        .env("HANGAR_STATE_DIR", state.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("warning: config ignored"));

    let fresh = state.path().join("sub/config.toml");
    let out = Command::new(env!("CARGO_BIN_EXE_hangar"))
        .args(["config", "--init"])
        .env("HANGAR_CONFIG", &fresh)
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = std::fs::read_to_string(&fresh).unwrap();
    assert!(text.contains("[refresh]"));
}
