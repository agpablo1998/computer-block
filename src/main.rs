// No console window on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    std::process::exit(computer_block::win::main());
}

#[cfg(not(windows))]
fn main() {
    eprintln!("computer-block only runs on Windows. The core logic is tested with `cargo test`.");
    std::process::exit(2);
}
