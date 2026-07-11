use std::path::PathBuf;

fn main() {
    let output = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .expect("output path");
    std::fs::write(
        output,
        "pub const NESTED_MESSAGE: &str = \"build.rs → nested Cargo → cached binary\";\n",
    )
    .expect("write generated Rust source");
}

