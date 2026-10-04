//! The daemon: a Unix-socket server around [`Scheduler`].
//!
//! A lease is held by its connection. When the connection ends without the
//! client reporting `Done` (the client crashed or was killed), the daemon
//! kills the job's process group before granting the machine again, so a
//! freed lease never leaves a build running.
use crate::now_ms;
use crate::protocol::{AdminAction, ClientEvent, Hello, Reply, Request};
use crate::scheduler::{Admission, Grant, Scheduler};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify, mpsc};

pub struct Options {
    pub socket: PathBuf,
    pub state_dir: PathBuf,
    pub config: crate::scheduler::Config,
    /// `owner/repo` whose merge queue ranks requests carrying `--pr`.
    pub merge_queue_repo: Option<String>,
}

struct Shared {
    scheduler: Mutex<Scheduler>,
    changed: Notify,
    /// Messages for a connection holding a lease (revocation, preemption).
    mailboxes: Mutex<HashMap<u64, mpsc::UnboundedSender<Reply>>>,
    log: std::sync::Mutex<std::fs::File>,
    secret: String,
}

#[derive(Serialize)]
struct LogEntry<'a> {
    at_ms: u64,
    event: &'a str,
    id: Option<u64>,
    machine: Option<&'a str>,
    label: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl Shared {
    fn log(
        &self,
        event: &str,
        id: Option<u64>,
        machine: Option<&str>,
        label: Option<&str>,
        detail: Option<String>,
    ) {
        let entry = LogEntry {
            at_ms: now_ms(),
            event,
            id,
            machine,
            label,
            detail,
        };
        if let Ok(line) = serde_json::to_string(&entry)
            && let Ok(mut file) = self.log.lock()
        {
            let _ = writeln!(file, "{line}");
        }
    }
}

/// The admin secret: created once, readable only by its owner.
pub fn admin_secret(state_dir: &Path) -> Result<String> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = state_dir.join("admin.token");
    if let Ok(secret) = std::fs::read_to_string(&path) {
        return Ok(secret.trim().to_owned());
    }
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| std::io::Read::read_exact(&mut random, &mut bytes))
        .context("read /dev/urandom")?;
    let secret = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("create {}", path.display()))?;
    writeln!(file, "{secret}")?;
    Ok(secret)
}

pub async fn serve(options: Options) -> Result<()> {
    std::fs::create_dir_all(&options.state_dir)
        .with_context(|| format!("create {}", options.state_dir.display()))?;
    let secret = admin_secret(&options.state_dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(options.state_dir.join("log.jsonl"))
        .context("open the lease log")?;
    // A stale socket from a crashed daemon; a live one refuses the bind below.
    if UnixStream::connect(&options.socket).await.is_ok() {
        anyhow::bail!(
            "another leased is listening on {}",
            options.socket.display()
        )
    }
    let _ = std::fs::remove_file(&options.socket);
    let listener = UnixListener::bind(&options.socket)
        .with_context(|| format!("bind {}", options.socket.display()))?;
    let shared = Arc::new(Shared {
        scheduler: Mutex::new(Scheduler::new(options.config)),
        changed: Notify::new(),
        mailboxes: Mutex::new(HashMap::new()),
        log: std::sync::Mutex::new(log),
        secret,
    });
    shared.log(
        "start",
        None,
        None,
        None,
        Some(options.socket.display().to_string()),
    );
    tokio::spawn(watch_overruns(Arc::clone(&shared)));
    if let Some(repo) = options.merge_queue_repo {
        tokio::spawn(watch_merge_queue(Arc::clone(&shared), repo));
    }
    eprintln!("leased listening on {}", options.socket.display());
    loop {
        let (stream, _) = listener.accept().await?;
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            if let Err(error) = connection(shared, stream).await {
                eprintln!("leased: connection: {error:#}");
            }
        });
    }
}

async fn send(writer: &mut tokio::net::unix::OwnedWriteHalf, reply: &Reply) -> Result<()> {
    let mut line = serde_json::to_vec(reply)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    Ok(())
}

async fn connection(shared: Arc<Shared>, stream: UnixStream) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let Some(first) = lines.next_line().await? else {
        return Ok(());
    };
    let hello: Hello = match serde_json::from_str(&first) {
        Ok(hello) => hello,
        Err(error) => {
            send(
                &mut writer,
                &Reply::Refused {
                    reason: format!("bad request: {error}"),
                },
            )
            .await?;
            return Ok(());
        }
    };
    match hello {
        Hello::Status => {
            let report = shared.scheduler.lock().await.status(now_ms());
            send(&mut writer, &Reply::Status(report)).await
        }
        Hello::CiStart { machine, job } => {
            let mut scheduler = shared.scheduler.lock().await;
            if !scheduler.knows(&machine) {
                return send(
                    &mut writer,
                    &Reply::Refused {
                        reason: format!("unknown machine {machine:?}"),
                    },
                )
                .await;
            }
            scheduler.ci_start(&machine, &job, now_ms());
            drop(scheduler);
            shared.log("ci_start", None, Some(&machine), Some(&job), None);
            send(&mut writer, &Reply::Ok).await
        }
        Hello::CiStop { machine, job } => {
            shared.scheduler.lock().await.ci_stop(&machine, &job);
            shared.log("ci_stop", None, Some(&machine), Some(&job), None);
            send(&mut writer, &Reply::Ok).await
        }
        Hello::Admin { secret, action } => {
            if secret != shared.secret {
                return send(
                    &mut writer,
                    &Reply::Refused {
                        reason: "wrong admin secret".into(),
                    },
                )
                .await;
            }
            let reply = admin(&shared, action).await;
            shared.changed.notify_waiters();
            send(&mut writer, &reply).await
        }
        Hello::Unleased {
            machine,
            label,
            id,
            reason,
        } => {
            shared.log("unleased", id, Some(&machine), Some(&label), Some(reason));
            send(&mut writer, &Reply::Ok).await
        }
        Hello::Request(request) => lease(shared, request, lines, writer).await,
    }
}

async fn admin(shared: &Shared, action: AdminAction) -> Reply {
    let mut scheduler = shared.scheduler.lock().await;
    let detail = format!("{action:?}");
    let ok = match &action {
        AdminAction::Reorder { id, position } => scheduler.reorder(*id, *position),
        AdminAction::Pause { machine } => {
            scheduler.set_paused(machine, true);
            scheduler.knows(machine)
        }
        AdminAction::Resume { machine } => {
            scheduler.set_paused(machine, false);
            scheduler.knows(machine)
        }
        AdminAction::Preempt { id } => {
            let held = scheduler.preempt(*id);
            if held && let Some(mailbox) = shared.mailboxes.lock().await.get(id) {
                let _ = mailbox.send(Reply::Preempting);
            }
            held
        }
        AdminAction::Reserve {
            machine,
            label,
            until_ms,
        } => {
            scheduler.reserve(machine, Some((label.clone(), *until_ms)));
            scheduler.knows(machine)
        }
        AdminAction::Unreserve { machine } => {
            scheduler.reserve(machine, None);
            true
        }
        AdminAction::Cancel { id } => {
            if scheduler.holds(*id) {
                // The client kills its job when told; the connection's end
                // then releases the lease (and kills the group if needed).
                if let Some(mailbox) = shared.mailboxes.lock().await.get(id) {
                    let _ = mailbox.send(Reply::Revoked {
                        reason: "cancelled by an administrator".into(),
                    });
                }
                true
            } else {
                let waiting = scheduler.is_waiting(*id);
                scheduler.release(*id);
                waiting
            }
        }
    };
    drop(scheduler);
    shared.log("admin", None, None, None, Some(detail));
    if ok {
        Reply::Ok
    } else {
        Reply::Refused {
            reason: "no such request, lease or machine".into(),
        }
    }
}

/// Grants whatever can be granted now and wakes every waiter to look.
async fn regrant(shared: &Shared) {
    let grants = shared.scheduler.lock().await.grant(now_ms());
    if !grants.is_empty() {
        shared.changed.notify_waiters();
    }
}

async fn lease(
    shared: Arc<Shared>,
    request: Request,
    mut lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    mut writer: tokio::net::unix::OwnedWriteHalf,
) -> Result<()> {
    let label = request.label.clone();
    let machine = request.machine.clone();
    let estimate = request.estimate_secs;
    let admission = shared.scheduler.lock().await.admit(request, now_ms());
    let (id, grant) = match admission {
        Admission::Refused(reason) => return send(&mut writer, &Reply::Refused { reason }).await,
        Admission::Nested(grant) => (grant.id, grant),
        Admission::Queued(id) => {
            shared.log(
                "request",
                Some(id),
                Some(&machine),
                Some(&label),
                Some(format!("estimate {estimate}s")),
            );
            match wait_for_grant(&shared, id, &machine, &mut lines, &mut writer).await? {
                Some(grant) => (id, grant),
                None => {
                    shared.scheduler.lock().await.release(id);
                    shared.log("abandon", Some(id), Some(&machine), Some(&label), None);
                    regrant(&shared).await;
                    return Ok(());
                }
            }
        }
    };
    shared.log(
        if grant.nested {
            "grant_nested"
        } else {
            "grant"
        },
        Some(id),
        Some(&machine),
        Some(&label),
        Some(format!("jobs {}", grant.jobs)),
    );
    let (mailbox, mut inbox) = mpsc::unbounded_channel();
    shared.mailboxes.lock().await.insert(id, mailbox);
    send(
        &mut writer,
        &Reply::Granted {
            id,
            jobs: grant.jobs,
            token: grant.token.clone(),
            nested: grant.nested,
        },
    )
    .await?;

    let started = std::time::Instant::now();
    let mut pgid = None;
    let mut code = None;
    loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => match serde_json::from_str::<ClientEvent>(&line) {
                    Ok(ClientEvent::Spawned { pgid: group }) => pgid = Some(group),
                    Ok(ClientEvent::Done { code: exit }) => {
                        code = Some(exit);
                        break;
                    }
                    Err(_) => {}
                },
                _ => break,
            },
            Some(reply) = inbox.recv() => {
                let _ = send(&mut writer, &reply).await;
            }
        }
    }
    shared.mailboxes.lock().await.remove(&id);
    if code.is_none()
        && let Some(group) = pgid
    {
        // The client went away while its job ran: end the job with the lease.
        kill_group(group);
        shared.log(
            "killed_orphan",
            Some(id),
            Some(&machine),
            Some(&label),
            Some(format!("pgid {group}")),
        );
    }
    shared.scheduler.lock().await.release(id);
    shared.log(
        if grant.nested {
            "release_nested"
        } else {
            "release"
        },
        Some(id),
        Some(&machine),
        Some(&label),
        Some(format!(
            "held {}s, exit {}",
            started.elapsed().as_secs(),
            code.map_or_else(|| "lost".to_owned(), |code| code.to_string())
        )),
    );
    regrant(&shared).await;
    Ok(())
}

/// Waits until `id` is granted, reporting its queue position as it changes.
/// `None` if the client disconnects first.
async fn wait_for_grant(
    shared: &Shared,
    id: u64,
    machine: &str,
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
) -> Result<Option<Grant>> {
    let mut last = None;
    loop {
        let changed = shared.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        {
            let mut scheduler = shared.scheduler.lock().await;
            // Another connection's call may have made this grant; claim it.
            let others = scheduler.grant(now_ms()).iter().any(|grant| grant.id != id);
            if others {
                shared.changed.notify_waiters();
            }
            if let Some(grant) = scheduler.claim(id) {
                return Ok(Some(grant));
            }
            if !scheduler.is_waiting(id) {
                drop(scheduler);
                send(
                    writer,
                    &Reply::Refused {
                        reason: "cancelled by an administrator".into(),
                    },
                )
                .await?;
                return Ok(None);
            }
            let position = scheduler.position(id).unwrap_or(0);
            let holder = scheduler.holder_label(machine);
            drop(scheduler);
            if last != Some((position, holder.clone())) {
                send(
                    writer,
                    &Reply::Queued {
                        id,
                        position,
                        holder: holder.clone(),
                    },
                )
                .await?;
                last = Some((position, holder));
            }
        }
        tokio::select! {
            _ = &mut changed => {}
            _ = tokio::time::sleep(Duration::from_secs(15)) => {}
            line = lines.next_line() => {
                if !matches!(line, Ok(Some(_))) {
                    return Ok(None);
                }
            }
        }
    }
}

fn kill_group(pgid: i32) {
    if pgid > 1 {
        // SAFETY: plain syscall; a group that already exited is harmless.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

async fn watch_overruns(shared: Arc<Shared>) {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        let over = shared.scheduler.lock().await.overruns(now_ms());
        for holder in over {
            shared.log(
                "overrun",
                Some(holder.id),
                Some(&holder.machine),
                Some(&holder.label),
                Some(format!("estimate {}s", holder.estimate_secs)),
            );
        }
    }
}

/// Polls the repository's merge queue for the positions that rank `--pr`.
async fn watch_merge_queue(shared: Arc<Shared>, repo: String) {
    let Some((owner, name)) = repo.split_once('/') else {
        eprintln!("leased: --merge-queue-repo must be owner/name");
        return;
    };
    let query = format!(
        "query {{ repository(owner: \"{owner}\", name: \"{name}\") {{ mergeQueue(branch: \"main\") {{ entries(first: 50) {{ nodes {{ position pullRequest {{ number }} }} }} }} }} }}"
    );
    loop {
        let output = tokio::process::Command::new("gh")
            .args(["api", "graphql", "-f"])
            .arg(format!("query={query}"))
            .output()
            .await;
        if let Ok(output) = output
            && output.status.success()
            && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
        {
            let positions = json
                .pointer("/data/repository/mergeQueue/entries/nodes")
                .and_then(|nodes| nodes.as_array())
                .map(|nodes| {
                    nodes
                        .iter()
                        .filter_map(|node| {
                            let pr = node.pointer("/pullRequest/number")?.as_u64()? as u32;
                            let position = node.get("position")?.as_u64()? as usize;
                            Some((pr, position))
                        })
                        .collect::<HashMap<_, _>>()
                })
                .unwrap_or_default();
            shared.scheduler.lock().await.set_merge_queue(positions);
            shared.changed.notify_waiters();
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
