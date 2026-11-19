//! The VST helper: runs one VST3 plugin for Ferrocast, in its own (locked-down) process.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(ferrocast_vsthost::helper::main(&args));
}
