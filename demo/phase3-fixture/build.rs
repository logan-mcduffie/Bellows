use std::path::PathBuf;
use std::process::Command;

fn main() {
    let output = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"))
        .join("nested_generated.rs");
    let status = Command::new("cargo")
        .args([
            "run",
            "--locked",
            "--offline",
            "--manifest-path",
            "generator/Cargo.toml",
            "--",
        ])
        .arg(&output)
        .status()
        .expect("launch nested Cargo generator");
    assert!(status.success(), "nested Cargo generator failed");
    println!("cargo:rerun-if-changed=generator");
}

