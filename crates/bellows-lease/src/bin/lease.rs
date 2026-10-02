//! `lease`: wait for a machine, run a command on it, release it on exit.
#[cfg(unix)]
mod client;

fn main() {
    #[cfg(unix)]
    std::process::exit(client::main());
    #[cfg(not(unix))]
    {
        eprintln!("lease runs on Unix only (the scheduler lives on the Linux laptop)");
        std::process::exit(2);
    }
}
