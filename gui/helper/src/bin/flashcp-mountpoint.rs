//! `/usr/libexec/flashcp-mountpoint` (pkexec): create or remove `/media/<user>/<NAME>`.

fn main() {
    std::process::exit(flashcp_helper::mountpoint::main());
}
