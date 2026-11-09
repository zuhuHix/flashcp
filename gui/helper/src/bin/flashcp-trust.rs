//! `/usr/libexec/flashcp-trust` (pkexec): add a USB drive's identity to the trust list.

fn main() {
    std::process::exit(flashcp_helper::trust_main());
}
