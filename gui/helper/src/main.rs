//! `/usr/libexec/flashcp-helper` (pkexec). All logic lives in the library so it can be tested
//! without root.

fn main() {
    std::process::exit(flashcp_helper::helper_main());
}
