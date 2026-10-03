//! bellowsd answers before it has walked its store (#33).
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn live(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    if stream
        .write_all(b"GET /live HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response.starts_with("HTTP/1.1 2")
}

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn stale_temp(data: &Path) -> std::path::PathBuf {
    let dir = data.join("blobs").join("ab");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(".tmp-1-2-3");
    fs::write(&path, b"interrupted write").unwrap();
    let two_hours_ago = SystemTime::now() - Duration::from_secs(2 * 60 * 60);
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(two_hours_ago)
        .unwrap();
    path
}

#[test]
fn answers_before_the_store_walk_and_still_removes_orphaned_temporaries() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("store");
    let orphan = stale_temp(&data);
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let walk_delay = Duration::from_secs(3);
    let started = Instant::now();
    let _daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_bellowsd"))
            .args(["--listen", &format!("127.0.0.1:{port}"), "--data-dir"])
            .arg(&data)
            // A stand-in for a walk over a large store (debug builds only).
            .env(
                "BELLOWSD_TEST_CLEANUP_DELAY_MS",
                walk_delay.as_millis().to_string(),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert!(
        wait_until(walk_delay, || live(port)),
        "bellowsd did not answer before its store walk finished"
    );
    assert!(started.elapsed() < walk_delay);
    assert!(orphan.exists(), "the walk ran before the socket was up");
    assert!(
        wait_until(Duration::from_secs(20), || !orphan.exists()),
        "the background walk never removed the orphaned temporary"
    );
    assert!(live(port), "bellowsd stopped answering during the walk");
}
