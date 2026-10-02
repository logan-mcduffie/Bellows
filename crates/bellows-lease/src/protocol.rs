//! Newline-delimited JSON messages over the daemon's Unix socket.
use serde::{Deserialize, Serialize};

/// The first message on a connection.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Hello {
    /// Ask for a machine; the connection then holds the lease once granted.
    Request(Request),
    Status,
    /// Note a CI job starting on a machine (it shares the machine).
    CiStart {
        machine: String,
        job: String,
    },
    CiStop {
        machine: String,
        job: String,
    },
    Admin {
        secret: String,
        action: AdminAction,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Request {
    pub machine: String,
    pub label: String,
    #[serde(default)]
    pub priority: i32,
    /// A pull request whose merge-queue position ranks this request.
    #[serde(default)]
    pub pr: Option<u32>,
    pub estimate_secs: u64,
    /// A session lease: hands out a token for nested runs.
    #[serde(default)]
    pub session: bool,
    /// The token of the session this request runs inside, if any.
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub client_pid: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AdminAction {
    /// Move a queued request to a position (0 = next).
    Reorder {
        id: u64,
        position: usize,
    },
    Pause {
        machine: String,
    },
    Resume {
        machine: String,
    },
    /// End a session at its next command boundary.
    Preempt {
        id: u64,
    },
    /// Only requests whose label contains `label` are granted until `until_ms`.
    Reserve {
        machine: String,
        label: String,
        until_ms: u64,
    },
    Unreserve {
        machine: String,
    },
    /// Remove a queued request, or revoke a held lease (its job is killed).
    Cancel {
        id: u64,
    },
}

/// Messages from the client after its lease is granted.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientEvent {
    /// The job started in this process group; it is killed if the lease ends
    /// without a `Done`.
    Spawned {
        pgid: i32,
    },
    Done {
        code: i32,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Queued {
        id: u64,
        position: usize,
        holder: Option<String>,
    },
    Granted {
        id: u64,
        jobs: u32,
        /// For a session: the token nested runs present.
        token: Option<String>,
        nested: bool,
    },
    Refused {
        reason: String,
    },
    /// The lease was revoked by an administrator: stop the job now.
    Revoked {
        reason: String,
    },
    /// The session ends at its next command boundary.
    Preempting,
    Status(StatusReport),
    Ok,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusReport {
    pub machines: Vec<MachineStatus>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MachineStatus {
    pub machine: String,
    pub paused: bool,
    pub reserved_for: Option<String>,
    pub ci_jobs: Vec<String>,
    pub jobs: u32,
    pub holder: Option<HolderView>,
    pub queue: Vec<QueuedView>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HolderView {
    pub id: u64,
    pub label: String,
    pub held_secs: u64,
    pub estimate_secs: u64,
    pub session: bool,
    pub preempting: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueuedView {
    pub id: u64,
    pub label: String,
    pub priority: i32,
    pub pr: Option<u32>,
    pub merge_queue_position: Option<usize>,
    pub waited_secs: u64,
    pub estimate_secs: u64,
}
