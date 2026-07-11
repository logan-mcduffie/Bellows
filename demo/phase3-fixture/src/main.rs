include!(concat!(env!("OUT_DIR"), "/nested_generated.rs"));

fn main() {
    println!("{NESTED_MESSAGE}");
}

