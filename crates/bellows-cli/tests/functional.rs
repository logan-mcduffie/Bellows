use bellows_core::PROTOCOL_VERSION;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

const BELLOWS: &str = env!("CARGO_BIN_EXE_bellows");

struct Fixture {
    temp: tempfile::TempDir,
    workspace: PathBuf,
    cache: PathBuf,
}
impl Fixture {
    fn new(lib: &str, main: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace with spaces");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname=\"fixture\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[workspace]\n",
        )
        .unwrap();
        fs::write(workspace.join("src/lib.rs"), lib).unwrap();
        fs::write(workspace.join("src/main.rs"), main).unwrap();
        let cache = temp.path().join("cache");
        Self {
            temp,
            workspace,
            cache,
        }
    }
    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut cmd = Command::new(program);
        cmd.current_dir(&self.workspace);
        for (name, _) in std::env::vars_os() {
            let name_str = name.to_string_lossy();
            if name_str.starts_with("BELLOWS_")
                || name_str.starts_with("CARGO_")
                || matches!(
                    name_str.as_ref(),
                    "RUSTC_WRAPPER"
                        | "RUSTC_WORKSPACE_WRAPPER"
                        | "RUSTFLAGS"
                        | "RUSTDOCFLAGS"
                        | "RUSTC"
                )
            {
                // Keep CARGO_HOME for the installed toolchain's normal lookup.
                if name_str != "CARGO_HOME" {
                    cmd.env_remove(name);
                }
            }
        }
        cmd.env("BELLOWS_COLOR", "never");
        cmd
    }
    fn local(&self) -> Command {
        let mut cmd = self.command(BELLOWS);
        cmd.args(["local", "--cache-dir"])
            .arg(&self.cache)
            .arg("--");
        cmd
    }
    fn build(&self) -> Output {
        checked(
            self.local()
                .args(["cargo", "build", "--release", "--offline"]),
        )
    }
    fn clean(&self) {
        fs::remove_dir_all(self.workspace.join("target")).unwrap();
    }
    fn value(&self, root: &str) -> String {
        let output = checked(
            &mut self.command(
                self.workspace
                    .join(root)
                    .join("release")
                    .join(format!("fixture{}", std::env::consts::EXE_SUFFIX)),
            ),
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
    fn explain(&self, flags: &[&str]) -> Value {
        let result = checked(
            self.command(BELLOWS)
                .args(["explain", "--local", "--cache-dir"])
                .arg(&self.cache)
                .args(flags)
                .arg("--json"),
        );
        serde_json::from_slice(&result.stdout).unwrap()
    }
    fn declared(&self) -> Command {
        let mut cmd = self.command(BELLOWS);
        cmd.args(["action", "run", "--local", "--cache-dir"])
            .arg(&self.cache)
            .args([
                "--name",
                "fixture",
                "--input",
                "Cargo.toml",
                "--input",
                "Cargo.lock",
                "--input",
                "src",
                "--output",
                "output",
            ]);
        cmd
    }
    fn lock(&self) {
        checked(
            self.command("cargo")
                .args(["generate-lockfile", "--offline"]),
        );
    }
}
fn checked(command: &mut Command) -> Output {
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{command:?}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    result
}
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn explicit_target_directories_share_verified_library_outputs() {
    let f = Fixture::new(
        "pub fn value() -> u32 { 42 }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    let first = f.temp.path().join("first target");
    let second = f.temp.path().join("second target");
    checked(
        f.local()
            .args(["cargo", "build", "--release", "--offline"])
            .env("CARGO_TARGET_DIR", &first),
    );
    let restored = checked(
        f.local()
            .args(["cargo", "build", "--release", "--offline"])
            .env("CARGO_TARGET_DIR", &second),
    );
    assert!(
        stderr(&restored).contains("LOCAL HIT"),
        "{}",
        stderr(&restored)
    );
    let output = checked(
        &mut f.command(
            second
                .join("release")
                .join(format!("fixture{}", std::env::consts::EXE_SUFFIX)),
        ),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
fn cargo_from_a_subdirectory_resolves_compiler_inputs_in_cargos_working_directory() {
    let f = Fixture::new("pub fn value() -> u32 { 42 }", "fn main() {}");
    let subdir = f.workspace.join("launcher");
    fs::create_dir(&subdir).unwrap();
    for warm in [false, true] {
        let output = checked(f.local().current_dir(&subdir).args([
            "cargo",
            "build",
            "--release",
            "--offline",
            "--manifest-path",
            "../Cargo.toml",
        ]));
        assert!(!stderr(&output).contains("FALLBACK"), "{}", stderr(&output));
        if warm {
            assert!(stderr(&output).contains("LOCAL HIT"), "{}", stderr(&output));
        }
        f.clean();
    }
}

#[test]
fn restored_cargo_json_artifact_messages_remain_valid() {
    let f = Fixture::new("pub fn value() -> u32 { 42 }", "fn main() {}");
    let target = f.temp.path().join("json target");
    for warm in [false, true] {
        let output = checked(
            f.local()
                .env(
                    "CARGO_TARGET_DIR",
                    target.to_string_lossy().replace('\\', "/"),
                )
                .args([
                    "cargo",
                    "build",
                    "--release",
                    "--offline",
                    "--message-format=json",
                ]),
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let messages: Vec<Value> = stdout
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            messages
                .iter()
                .any(|v| v["reason"] == "build-finished" && v["success"] == true)
        );
        for message in messages
            .iter()
            .filter(|v| v["reason"] == "compiler-artifact")
        {
            for path in message["filenames"].as_array().unwrap() {
                assert!(std::path::Path::new(path.as_str().unwrap()).is_file());
            }
        }
        if warm {
            assert!(String::from_utf8_lossy(&output.stderr).contains("LOCAL HIT"));
        }
        fs::remove_dir_all(&target).unwrap();
    }
}

#[cfg(any(unix, windows))]
fn link_directory(f: &Fixture, source: &std::path::Path, destination: &std::path::Path) {
    #[cfg(unix)]
    {
        let _ = f;
        std::os::unix::fs::symlink(source, destination).unwrap();
    }
    #[cfg(windows)]
    {
        // NTFS junctions need neither administrator rights nor Developer Mode.
        checked(
            f.command("cmd")
                .args(["/D", "/C", "mklink", "/J"])
                .arg(destination)
                .arg(source),
        );
    }
}

#[cfg(windows)]
#[test]
fn junction_retargeting_never_restores_old_code() {
    let f = Fixture::new(
        "#[path=\"../selected/value.rs\"] mod selected; pub fn value() -> u32 { selected::value() }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    let link = f.workspace.join("selected");
    for value in [1, 2] {
        let source = f.temp.path().join(format!("source-{value}"));
        fs::create_dir(&source).unwrap();
        fs::write(
            source.join("value.rs"),
            format!("pub fn value() -> u32 {{ {value} }}"),
        )
        .unwrap();
        link_directory(&f, &source, &link);
        assert!(stderr(&f.build()).contains("symlinked compiler input"));
        assert_eq!(f.value("target"), value.to_string());
        fs::remove_dir(&link).unwrap();
        f.clean();
    }
}

#[test]
fn embedded_environment_paths_are_not_reused_between_checkouts() {
    let mut f = Fixture::new(
        "pub fn value() -> &'static str { env!(\"CARGO_MANIFEST_DIR\") }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    f.build();
    assert_eq!(f.value("target"), f.workspace.to_str().unwrap());
    let other = f.temp.path().join("other");
    fs::create_dir_all(other.join("src")).unwrap();
    for path in ["Cargo.toml", "Cargo.lock", "src/lib.rs", "src/main.rs"] {
        fs::copy(f.workspace.join(path), other.join(path)).unwrap();
    }
    f.workspace = other;
    let result = f.build();
    assert_eq!(f.value("target"), f.workspace.to_str().unwrap());
    assert!(stderr(&result).contains("environment changed: CARGO_MANIFEST_DIR"));
    f.clean();
    let warm = stderr(&f.build());
    assert!(warm.contains("LOCAL HIT"), "{warm}");
}

#[cfg(unix)]
#[test]
fn symlink_retargeting_never_restores_old_code() {
    let f = Fixture::new(
        "mod selected; pub fn value() -> u32 { selected::value() }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    fs::write(
        f.workspace.join("src/one.rs"),
        "pub fn value() -> u32 { 1 }",
    )
    .unwrap();
    fs::write(
        f.workspace.join("src/two.rs"),
        "pub fn value() -> u32 { 2 }",
    )
    .unwrap();
    let link = f.workspace.join("src/selected.rs");
    std::os::unix::fs::symlink("one.rs", &link).unwrap();
    assert!(stderr(&f.build()).contains("symlinked compiler input"));
    assert_eq!(f.value("target"), "1");
    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink("two.rs", &link).unwrap();
    f.clean();
    f.build();
    assert_eq!(f.value("target"), "2");
}

#[test]
fn unsupported_emit_products_are_produced_on_every_invocation() {
    let f = Fixture::new("pub fn value() -> u32 { 1 }", "fn main() {}");
    for _ in 0..2 {
        fs::create_dir_all(f.workspace.join("out")).unwrap();
        let result = checked(f.local().arg(BELLOWS).args([
            "rustc",
            "--crate-name",
            "fixture",
            "--crate-type",
            "rlib",
            "--emit=dep-info,metadata,llvm-ir",
            "-C",
            "extra-filename=-audit",
            "--out-dir",
            "out",
            "src/lib.rs",
        ]));
        assert!(stderr(&result).contains("unsupported emit set"));
        assert!(f.workspace.join("out/fixture-audit.ll").is_file());
        fs::remove_dir_all(f.workspace.join("out")).unwrap();
    }
}

#[test]
fn declared_actions_preserve_environment_and_configuration_rustflags() {
    let f = Fixture::new(
        "pub fn value() -> bool { cfg!(audit_flag) }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    f.lock();
    for (name, value) in [
        ("RUSTFLAGS", "--cfg audit_flag"),
        ("CARGO_ENCODED_RUSTFLAGS", "--cfg\u{1f}audit_flag"),
    ] {
        checked(
            f.declared()
                .args([
                    "--env",
                    name,
                    "--",
                    "cargo",
                    "build",
                    "--release",
                    "--locked",
                    "--offline",
                    "--target-dir",
                    "output",
                ])
                .env(name, value),
        );
        assert_eq!(f.value("output"), "true");
    }
    fs::create_dir(f.workspace.join(".cargo")).unwrap();
    fs::write(
        f.workspace.join(".cargo/config.toml"),
        "[build]\nrustflags = [\"--cfg\", \"audit_flag\"]\n",
    )
    .unwrap();
    checked(f.declared().args([
        "--input",
        ".cargo",
        "--",
        "cargo",
        "build",
        "--release",
        "--locked",
        "--offline",
        "--target-dir",
        "output",
    ]));
    assert_eq!(f.value("output"), "true");
}

#[test]
fn declared_roots_replace_stale_files_and_allow_nested_declarations() {
    let f = Fixture::new("pub fn value() {}", "fn main() {}");
    f.lock();
    let build = || {
        checked(f.declared().args([
            "--output",
            "output/release",
            "--",
            "cargo",
            "build",
            "--release",
            "--locked",
            "--offline",
            "--target-dir",
            "output",
        ]))
    };
    build();
    fs::write(f.workspace.join("output/stale"), "obsolete artifact").unwrap();
    fs::write(f.workspace.join("unrelated"), "keep me").unwrap();
    assert!(String::from_utf8_lossy(&build().stdout).contains("CACHE HIT"));
    assert!(!f.workspace.join("output/stale").exists());
    assert!(f.workspace.join("unrelated").exists());
    assert!(
        f.workspace
            .join("output/release")
            .join(format!("fixture{}", std::env::consts::EXE_SUFFIX))
            .is_file()
    );
    let rejected = f
        .declared()
        .args([
            "--output",
            "src",
            "--",
            "cargo",
            "build",
            "--release",
            "--locked",
            "--offline",
            "--target-dir",
            "output",
        ])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(stderr(&rejected).contains("input/output overlap"));
    assert!(f.workspace.join("src/lib.rs").exists());
}

#[test]
fn broken_local_cache_does_not_prevent_cargo_from_running() {
    let f = Fixture::new("", "fn main() {}");
    fs::write(&f.cache, "not a directory").unwrap();
    let result = checked(f.local().args(["cargo", "--version"]));
    assert!(String::from_utf8_lossy(&result.stdout).starts_with("cargo "));
    assert!(stderr(&result).contains("cache initialization failed"));
    // The wrapped command's own failure status is still returned.
    let result = f
        .local()
        .args(["cargo", "not-a-real-subcommand"])
        .output()
        .unwrap();
    assert!(!result.status.success());
}

#[test]
fn diagnostics_distinguish_source_environment_flags_corruption_and_sessions() {
    let f = Fixture::new(
        "mod value; pub fn value() -> u32 { value::value() }",
        "fn main() {}",
    );
    fs::write(
        f.workspace.join("src/value.rs"),
        "pub fn value() -> u32 { 1 }",
    )
    .unwrap();
    f.build();
    let first = f.explain(&["--latest", "--summary"]);
    assert_eq!(first["decisions"]["miss"], 1);
    let first_id = first["session_ids"][0].as_str().unwrap().to_owned();
    f.clean();
    let warm = stderr(&f.build());
    assert!(warm.contains("LOCAL HIT"), "{warm}");
    let summary = f.explain(&["--latest", "--summary"]);
    assert_eq!(summary["decisions"]["l1_hit"], 1);
    assert!(summary["decisions"].get("miss").is_none());
    assert_eq!(
        f.explain(&["--session", &first_id, "--summary"])["decisions"]["miss"],
        1
    );
    fs::write(
        f.workspace.join("src/value.rs"),
        "pub fn value() -> u32 { 2 }",
    )
    .unwrap();
    f.clean();
    let edited = stderr(&f.build());
    assert!(
        edited.contains(&format!(
            "input changed: $WORKSPACE{0}src{0}value.rs",
            std::path::MAIN_SEPARATOR
        )),
        "{edited}"
    );
    let events = f.explain(&["--latest", "--crate", "fixture"]);
    assert!(
        events
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["reason"] == "input_changed")
    );
    // A primary source edit changes the static key; explain the component delta.
    fs::write(
        f.workspace.join("src/lib.rs"),
        "mod value; pub fn value() -> u32 { value::value() + 1 }",
    )
    .unwrap();
    f.clean();
    let edited = stderr(&f.build());
    assert!(
        edited.contains(&format!(
            "explicit input changed: $WORKSPACE{0}src{0}lib.rs",
            std::path::MAIN_SEPARATOR
        )),
        "{edited}"
    );
    f.clean();
    let flags = checked(
        f.local()
            .args(["cargo", "build", "--release", "--offline"])
            .env("RUSTFLAGS", "--cfg private_value_123"),
    );
    assert!(stderr(&flags).contains("environment changed: RUSTFLAGS"));
    let latest = f.explain(&["--latest", "--summary"]);
    assert!(!latest.to_string().contains("private_value_123"));
    // Restore the unflagged identity, then damage its artifact.
    f.clean();
    f.build();
    let store =
        bellows_core::Store::open(f.cache.join(format!("store-v{PROTOCOL_VERSION}"))).unwrap();
    let events: Vec<bellows_core::Event> = fs::read_to_string(f.cache.join("events.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let key = events
        .iter()
        .rev()
        .find(|e| e.kind == "l1_hit")
        .unwrap()
        .static_key
        .as_ref()
        .unwrap();
    let index = store.read_candidates(key).unwrap();
    let artifact = index.candidates[0]
        .artifacts
        .iter()
        .find(|a| a.file_name.ends_with(".rlib"))
        .unwrap();
    fs::write(store.blob_path(&artifact.digest).unwrap(), "broken").unwrap();
    f.clean();
    let rebuilt = f.build();
    assert!(stderr(&rebuilt).contains("corrupt blob"));
    assert!(
        f.explain(&["--latest", "--summary"])["decisions"]["corrupt"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[test]
fn explain_finds_default_local_log_and_honors_event_log_override() {
    let f = Fixture::new("pub fn value() {}", "fn main() {}");
    checked(
        f.command(BELLOWS)
            .args(["cargo", "build", "--release", "--offline"])
            .env("XDG_CACHE_HOME", f.temp.path().join("xdg")),
    );
    let result = checked(
        f.command(BELLOWS)
            .args(["explain", "--latest", "--json"])
            .env("XDG_CACHE_HOME", f.temp.path().join("xdg")),
    );
    assert!(
        !serde_json::from_slice::<Vec<Value>>(&result.stdout)
            .unwrap()
            .is_empty()
    );
    let path = f.temp.path().join("custom.jsonl");
    checked(
        f.local()
            .args(["cargo", "check", "--release", "--offline"])
            .env("BELLOWS_EVENT_LOG", &path),
    );
    let result = checked(
        f.command(BELLOWS)
            .args(["stats", "--local", "--cache-dir"])
            .arg(&f.cache)
            .args(["--latest", "--json"])
            .env("BELLOWS_EVENT_LOG", &path),
    );
    assert!(
        !serde_json::from_slice::<Value>(&result.stdout).unwrap()["events"]
            .as_object()
            .unwrap()
            .is_empty()
    );
}

#[cfg(any(unix, windows))]
#[test]
fn restore_refuses_symlink_destinations_without_touching_other_outputs() {
    let f = Fixture::new("pub fn value() {}", "fn main() {}");
    f.lock();
    let build = || {
        f.declared()
            .args([
                "--",
                "cargo",
                "build",
                "--release",
                "--locked",
                "--offline",
                "--target-dir",
                "output",
            ])
            .output()
            .unwrap()
    };
    assert!(build().status.success());
    fs::remove_dir_all(f.workspace.join("output")).unwrap();
    let elsewhere = f.temp.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::write(elsewhere.join("keep"), "untouched").unwrap();
    link_directory(&f, &elsewhere, &f.workspace.join("output"));
    let rejected = build();
    assert!(!rejected.status.success());
    assert!(!stderr(&rejected).contains("stale cached result will be rebuilt"));
    assert_eq!(
        fs::read_to_string(elsewhere.join("keep")).unwrap(),
        "untouched"
    );
    assert!(!elsewhere.join("release").exists());
}

#[test]
fn cfg_literals_containing_checkout_paths_are_hashed_exactly() {
    let mut f = Fixture::new("", "fn main() { println!(\"{}\", fixture::value()); }");
    let first = f.workspace.clone();
    fs::write(
        first.join("src/lib.rs"),
        format!(
            "pub fn value() -> bool {{ cfg!(audit_path={:?}) }}",
            first.to_string_lossy()
        ),
    )
    .unwrap();
    checked(
        f.local()
            .args(["cargo", "build", "--release", "--offline"])
            .env(
                "CARGO_ENCODED_RUSTFLAGS",
                format!("--cfg\u{1f}audit_path={:?}", first.to_string_lossy()),
            ),
    );
    assert_eq!(f.value("target"), "true");
    let other = f.temp.path().join("other");
    fs::create_dir_all(other.join("src")).unwrap();
    for path in ["Cargo.toml", "Cargo.lock", "src/lib.rs", "src/main.rs"] {
        fs::copy(first.join(path), other.join(path)).unwrap();
    }
    f.workspace = other;
    let result = checked(
        f.local()
            .args(["cargo", "build", "--release", "--offline"])
            .env(
                "CARGO_ENCODED_RUSTFLAGS",
                format!("--cfg\u{1f}audit_path={:?}", f.workspace.to_string_lossy()),
            ),
    );
    assert!(!stderr(&result).contains("LOCAL HIT"));
    assert_eq!(f.value("target"), "false");
    assert!(stderr(&result).contains("argument --cfg"));
}

#[cfg(unix)]
#[test]
fn native_archives_on_unqualified_and_all_search_paths_never_hit() {
    let f = Fixture::new(
        "#[link(name=\"audit_native\", kind=\"static\")] unsafe extern \"C\" { fn native_value() -> i32; } pub fn value() -> i32 { unsafe { native_value() } }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    fs::create_dir(f.workspace.join("native")).unwrap();
    for search in ["native", "all=native"] {
        for value in [1, 2] {
            fs::write(
                f.workspace.join("native/value.c"),
                format!("int native_value(void) {{ return {value}; }}"),
            )
            .unwrap();
            checked(
                f.command("cc")
                    .args(["-c", "native/value.c", "-o", "native/value.o"]),
            );
            checked(
                f.command("ar")
                    .args(["rcs", "native/libaudit_native.a", "native/value.o"]),
            );
            fs::create_dir_all(f.workspace.join("out")).unwrap();
            let result = checked(f.local().arg(BELLOWS).args([
                "rustc",
                "src/lib.rs",
                "--edition=2024",
                "--crate-name=fixture",
                "--crate-type=rlib",
                "--emit=dep-info,link",
                "-Cextra-filename=-audit",
                "--out-dir=out",
                "-L",
                search,
            ]));
            assert!(stderr(&result).contains("native linker or external codegen inputs"));
            assert!(!stderr(&result).contains("LOCAL HIT"));
            checked(f.command("rustc").args([
                "src/main.rs",
                "--edition=2024",
                "--extern",
                "fixture=out/libfixture-audit.rlib",
                "-o",
                "app",
            ]));
            let result = checked(&mut f.command(f.workspace.join("app")));
            assert_eq!(
                String::from_utf8(result.stdout).unwrap().trim(),
                value.to_string()
            );
            fs::remove_dir_all(f.workspace.join("out")).unwrap();
        }
    }
}

#[test]
fn waiting_build_reacquires_released_lease_without_timing_out() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    let f = Fixture::new(
        "pub fn value() -> u32 { 42 }",
        "fn main() { println!(\"{}\", fixture::value()); }",
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server_url = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let lease_requests = Arc::new(AtomicUsize::new(0));
    let stop = stopped.clone();
    let requests = lease_requests.clone();
    let server = std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("accept: {error}"),
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<u64>().unwrap();
                }
            }
            std::io::copy(&mut reader.take(length), &mut std::io::sink()).unwrap();
            let (status, body) = if request.starts_with("POST /v1/leases/") {
                let attempt = requests.fetch_add(1, Ordering::SeqCst);
                let response = if attempt == 0 {
                    serde_json::json!({"status":"wait","retry_after_ms":50,"expires_ms":bellows_core::now_ms()+60_000})
                } else {
                    serde_json::json!({"status":"owned","token":"test-lease","expires_ms":bellows_core::now_ms()+60_000})
                };
                ("200 OK", response.to_string())
            } else if request.starts_with("GET /v1/actions/") {
                ("200 OK", "{\"candidates\":[]}".to_owned())
            } else if request.starts_with("HEAD ") {
                ("404 Not Found", String::new())
            } else {
                ("200 OK", "{}".to_owned())
            };
            write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    let events = f.temp.path().join("events.jsonl");
    let output = f
        .command(BELLOWS)
        .env("BELLOWS_L1", "0")
        .env("BELLOWS_STATE_DIR", f.temp.path().join("state"))
        .env("BELLOWS_EVENT_LOG", &events)
        .env("BELLOWS_MAX_WAIT_MS", "1000")
        .args([
            "run",
            "--server",
            &server_url,
            "--",
            "cargo",
            "build",
            "--release",
            "--offline",
        ])
        .output()
        .unwrap();
    stopped.store(true, Ordering::SeqCst);
    server.join().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(lease_requests.load(Ordering::SeqCst), 2);
    let recorded = fs::read_to_string(events).unwrap();
    assert!(
        recorded.contains("\"kind\":\"lease_acquired\""),
        "{recorded}"
    );
    assert!(!recorded.contains("\"kind\":\"fallback\""), "{recorded}");
    assert_eq!(f.value("target"), "42");
}
