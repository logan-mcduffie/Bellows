//! End to end: a real daemon, real clients, real process groups.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon {
    child: Child,
    socket: PathBuf,
    state: PathBuf,
    dir: tempfile::TempDir,
}

impl Daemon {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("lease.sock");
        let state = dir.path().join("state");
        let child = Command::new(env!("CARGO_BIN_EXE_leased"))
            .arg("--socket")
            .arg(&socket)
            .arg("--state-dir")
            .arg(&state)
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            assert!(Instant::now() < deadline, "leased did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        Self {
            child,
            socket,
            state,
            dir,
        }
    }

    fn lease(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lease"));
        command
            .env("LEASE_SOCKET", &self.socket)
            .env("LEASE_STATE_DIR", &self.state)
            .env_remove("LEASE_TOKEN")
            .env_remove("LEASE_MACHINE");
        command
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.state.join("log.jsonl")).unwrap_or_default()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run_shell(daemon: &Daemon, label: &str, script: &str) -> Child {
    daemon
        .lease()
        .args([
            "run", "laptop", "--est", "1m", "--label", label, "--", "sh", "-c", script,
        ])
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks for existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
fn leases_on_one_machine_never_overlap() {
    let daemon = Daemon::start();
    let trace = daemon.dir.path().join("trace");
    let script = |name: &str| {
        format!(
            "echo start-{name} >> {t}; sleep 0.4; echo end-{name} >> {t}",
            t = trace.display()
        )
    };
    let mut clients = ["a", "b", "c"]
        .map(|name| run_shell(&daemon, name, &script(name)))
        .into_iter()
        .collect::<Vec<_>>();
    for client in &mut clients {
        assert!(client.wait().unwrap().success());
    }
    let lines = std::fs::read_to_string(&trace).unwrap();
    let lines = lines.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 6, "{lines:?}");
    for pair in lines.chunks(2) {
        let name = pair[0].strip_prefix("start-").expect("a start");
        assert_eq!(pair[1], format!("end-{name}"), "jobs overlapped: {lines:?}");
    }
    let log = daemon.log();
    assert_eq!(log.matches("\"event\":\"grant\"").count(), 3, "{log}");
    assert_eq!(log.matches("\"event\":\"release\"").count(), 3, "{log}");
}

#[test]
fn a_killed_client_takes_its_job_with_it_and_frees_the_machine() {
    let daemon = Daemon::start();
    let pid_file = daemon.dir.path().join("job.pid");
    let mut client = run_shell(
        &daemon,
        "doomed",
        &format!("sleep 60 & echo $! > {}; wait", pid_file.display()),
    );
    wait_until("the job to start", || {
        std::fs::read_to_string(&pid_file).is_ok_and(|text| text.ends_with('\n'))
    });
    let job: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(alive(job));
    // SIGKILL: the client gets no chance to clean up.
    client.kill().unwrap();
    client.wait().unwrap();
    wait_until("the orphaned job to die", || !alive(job));
    let mut next = run_shell(&daemon, "next", "true");
    assert!(next.wait().unwrap().success(), "the machine was freed");
    assert!(daemon.log().contains("killed_orphan"));
}

#[test]
fn a_session_runs_its_nested_leases_without_queueing() {
    let daemon = Daemon::start();
    let lease = env!("CARGO_BIN_EXE_lease");
    let status = daemon
        .lease()
        .args(["hold", "laptop", "--est", "1m", "--label", "batch", "--", "sh", "-c"])
        .arg(format!(
            "{lease} run laptop --est 1m --label step1 -- true && {lease} run laptop --est 1m --label step2 -- true"
        ))
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let log = daemon.log();
    assert_eq!(log.matches("grant_nested").count(), 2, "{log}");
}

#[test]
fn status_admin_and_ci_sharing() {
    let daemon = Daemon::start();
    let ok = |args: &[&str]| daemon.lease().args(args).status().unwrap().success();
    assert!(ok(&["ci-start", "laptop", "--job", "run-1/gpu"]));
    let status = daemon.lease().args(["status", "--json"]).output().unwrap();
    let report: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(report["machines"][0]["jobs"], 12);
    assert!(ok(&["ci-stop", "laptop", "--job", "run-1/gpu"]));
    assert!(ok(&["admin", "pause", "desktop"]));
    let report: serde_json::Value = serde_json::from_slice(
        &daemon
            .lease()
            .args(["status", "--json"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_eq!(report["machines"][1]["paused"], true);
    assert_eq!(report["machines"][0]["jobs"], 24);
    // The job sees the grant's job count.
    let out = daemon
        .lease()
        .args([
            "run",
            "laptop",
            "--est",
            "1m",
            "--label",
            "jobs",
            "--",
            "sh",
            "-c",
            "echo $CARGO_BUILD_JOBS",
        ])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "24");
    // A wrong secret is refused.
    let refused = daemon
        .lease()
        .env("LEASE_STATE_DIR", Path::new("/nonexistent"))
        .args(["admin", "resume", "desktop"])
        .status()
        .unwrap();
    assert!(!refused.success());
}

#[test]
fn an_exit_code_passes_through() {
    let daemon = Daemon::start();
    let mut client = run_shell(&daemon, "fails", "exit 7");
    assert_eq!(client.wait().unwrap().code(), Some(7));
}
