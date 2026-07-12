use std::env;
use std::io::{IsTerminal, stderr, stdout};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "1";
const DIM: &str = "2";
const RED: &str = "1;31";
const GREEN: &str = "1;32";
const YELLOW: &str = "1;33";
const BLUE: &str = "1;34";
const MAGENTA: &str = "1;35";
const CYAN: &str = "1;36";

pub fn stdout_color() -> bool {
    color_enabled(stdout().is_terminal())
}

pub fn stderr_color() -> bool {
    color_enabled(stderr().is_terminal())
}

fn color_enabled(is_terminal: bool) -> bool {
    let mode = env::var("BELLOWS_COLOR").unwrap_or_else(|_| "auto".into());
    decide_color(
        &mode,
        env::var_os("NO_COLOR").is_some(),
        env::var("TERM").as_deref() == Ok("dumb"),
        is_terminal,
        color_ci(),
    )
}

fn decide_color(
    mode: &str,
    no_color: bool,
    dumb_terminal: bool,
    is_terminal: bool,
    color_ci: bool,
) -> bool {
    match mode.to_ascii_lowercase().as_str() {
        "always" => true,
        "never" => false,
        _ => !no_color && !dumb_terminal && (is_terminal || color_ci),
    }
}

fn color_ci() -> bool {
    [
        "GITHUB_ACTIONS",
        "GITLAB_CI",
        "BUILDKITE",
        "CIRCLECI",
        "TF_BUILD",
    ]
    .iter()
    .any(|name| env::var_os(name).is_some())
}

fn paint(enabled: bool, style: &str, text: impl AsRef<str>) -> String {
    let text = text.as_ref();
    if enabled {
        format!("\x1b[{style}m{text}{RESET}")
    } else {
        text.to_owned()
    }
}

fn event_label(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "hit" => ("CACHE HIT", GREEN),
        "l1_hit" => ("LOCAL HIT", GREEN),
        "miss" => ("CACHE MISS", YELLOW),
        "bypass" => ("BYPASS", CYAN),
        "fallback" => ("FALLBACK", MAGENTA),
        "wait" => ("WAITING", BLUE),
        "single_flight" => ("SHARED HIT", GREEN),
        "corrupt" | "candidate_rejected" => ("REJECTED", RED),
        "store" => ("STORED", BLUE),
        "running" => ("RUNNING", CYAN),
        "published" => ("PUBLISHED", GREEN),
        "restored" => ("RESTORED", GREEN),
        "executed" => ("EXECUTED", BLUE),
        "captured" => ("CAPTURED", BLUE),
        "gc" => ("COLLECTED", GREEN),
        _ => ("BELLOWS", CYAN),
    }
}

pub fn status(enabled: bool, kind: &str, subject: &str, detail: &str) -> String {
    let (label, color) = event_label(kind);
    let label = paint(enabled, color, format!("{label:>11}"));
    let subject = paint(enabled, BOLD, subject);
    let detail = paint(enabled, DIM, detail);
    if detail.is_empty() {
        format!("{label} {subject}")
    } else {
        format!("{label} {subject}  {detail}")
    }
}

pub fn success(enabled: bool, subject: &str, detail: &str) -> String {
    let check = paint(enabled, GREEN, "✓");
    let subject = paint(enabled, BOLD, subject);
    let detail = paint(enabled, DIM, detail);
    if detail.is_empty() {
        format!("{check} {subject}")
    } else {
        format!("{check} {subject}  {detail}")
    }
}

pub fn warning(enabled: bool, subject: &str, detail: &str) -> String {
    status(enabled, "fallback", subject, detail)
}

pub fn error(enabled: bool, detail: &str) -> String {
    format!(
        "{} {}",
        paint(enabled, RED, format!("{:>11}", "ERROR")),
        detail
    )
}

pub fn heading(enabled: bool, title: &str) -> String {
    paint(enabled, CYAN, title)
}

pub fn section(enabled: bool, title: &str) -> String {
    format!("\n{}", paint(enabled, BOLD, title))
}

pub fn key_value(enabled: bool, key: &str, value: impl std::fmt::Display) -> String {
    format!("  {}  {value}", paint(enabled, DIM, format!("{key:<20}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_lines_are_aligned_without_color() {
        assert_eq!(
            status(false, "hit", "serde", "restored remote compiler result"),
            "  CACHE HIT serde  restored remote compiler result"
        );
        assert_eq!(
            status(false, "miss", "manifold-engine", "no candidate"),
            " CACHE MISS manifold-engine  no candidate"
        );
    }

    #[test]
    fn event_lines_apply_semantic_ansi_styles() {
        let line = status(true, "fallback", "serde", "remote unavailable");
        assert!(line.contains("\x1b[1;35m   FALLBACK\x1b[0m"));
        assert!(line.contains("\x1b[1mserde\x1b[0m"));
        assert!(line.contains("\x1b[2mremote unavailable\x1b[0m"));
    }

    #[test]
    fn unknown_events_remain_readable() {
        assert_eq!(
            status(false, "future-event", "crate", "detail"),
            "    BELLOWS crate  detail"
        );
    }

    #[test]
    fn color_policy_honors_term_ci_and_explicit_overrides() {
        assert!(decide_color("auto", false, false, true, false));
        assert!(decide_color("auto", false, false, false, true));
        assert!(!decide_color("auto", true, false, true, true));
        assert!(!decide_color("auto", false, true, true, true));
        assert!(decide_color("always", true, true, false, false));
        assert!(!decide_color("never", false, false, true, true));
    }
}
