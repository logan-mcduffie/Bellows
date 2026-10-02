//! `leased`: the lease scheduler daemon.
#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    use clap::Parser;
    use std::path::PathBuf;

    #[derive(Parser)]
    #[command(name = "leased", version, about = "Machine lease scheduler daemon")]
    struct Args {
        #[arg(long, env = "LEASE_SOCKET")]
        socket: Option<PathBuf>,
        /// Audit log and admin token.
        #[arg(long, env = "LEASE_STATE_DIR")]
        state_dir: Option<PathBuf>,
        #[arg(long, value_delimiter = ',', default_value = "laptop,desktop")]
        machines: Vec<String>,
        /// Build jobs per grant on an idle machine.
        #[arg(long, default_value_t = 24)]
        jobs: u32,
        /// Build jobs per grant while a CI job shares the machine.
        #[arg(long, default_value_t = 12)]
        ci_jobs: u32,
        /// `owner/name` whose merge queue ranks `--pr` requests (needs `gh`).
        #[arg(long)]
        merge_queue_repo: Option<String>,
    }

    let args = Args::parse();
    let options = bellows_lease::server::Options {
        socket: args.socket.unwrap_or_else(bellows_lease::default_socket),
        state_dir: args
            .state_dir
            .unwrap_or_else(bellows_lease::default_state_dir),
        config: bellows_lease::scheduler::Config {
            machines: args.machines,
            jobs: args.jobs,
            ci_jobs: args.ci_jobs,
            ..Default::default()
        },
        merge_queue_repo: args.merge_queue_repo,
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(bellows_lease::server::serve(options))
}

#[cfg(not(unix))]
fn main() {
    eprintln!("leased runs on Unix only (the scheduler lives on the Linux laptop)");
    std::process::exit(2);
}
