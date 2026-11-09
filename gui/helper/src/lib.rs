//! flashcp-helper: the only privileged piece of the flashcp desktop app.
//!
//! Started as root by pkexec, it validates a request, checks that the target is an idle USB
//! drive, makes the invoking user the owner of that one device node, runs `/usr/bin/flashcp` as
//! that user, and gives the node back. See packaging/desktop/SECURITY-NOTES.md.

pub mod args;
pub mod device;
pub mod exec;
pub mod json;
pub mod mounts;

use std::ffi::OsString;
use std::process::Stdio;

use device::{validate_device, RealStat, Roots};
use exec::{ExecPlan, PinnedNode};

/// Exit code for a refused request.
pub const EXIT_VALIDATION: i32 = 3;
/// Exit code for an unexpected failure inside the helper.
pub const EXIT_INTERNAL: i32 = 4;

enum Fail {
    Validation(String),
    Internal(String),
}

/// Run the helper; returns the process exit code (flashcp's own, or 3/4 for helper failures).
pub fn run_main() -> i32 {
    match real_main() {
        Ok(code) => code,
        Err(Fail::Validation(m)) => {
            json::print_error(&m);
            EXIT_VALIDATION
        }
        Err(Fail::Internal(m)) => {
            json::print_error(&m);
            EXIT_INTERNAL
        }
    }
}

fn real_main() -> Result<i32, Fail> {
    use Fail::{Internal, Validation};

    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    let req = args::parse(&argv).map_err(Validation)?;

    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err(Validation("must be started via pkexec".into()));
    }
    let pkexec_uid = std::env::var("PKEXEC_UID").ok();
    let sudo_uid = std::env::var("SUDO_UID").ok();
    let uid = exec::invoking_uid(pkexec_uid.as_deref(), sudo_uid.as_deref()).map_err(Validation)?;
    let creds = exec::lookup_user(uid).map_err(Validation)?;

    let roots = Roots::system();
    let valid = validate_device(&roots, &RealStat, req.dev()).map_err(Validation)?;

    // From here until the guard is dropped, SIGINT/SIGTERM/SIGHUP/SIGQUIT are held and forwarded
    // to flashcp, so they cannot skip the ownership restore.
    exec::block_signals().map_err(|e| Internal(format!("sigprocmask: {e}")))?;

    // Pin the node, confirm it is still the device we validated, then validate again right
    // before granting so the window between check and use is as small as possible.
    let node = PinnedNode::open(&roots.dev.join(&valid.name.name))
        .map_err(|e| Validation(format!("{}: {e}", valid.name.path())))?;
    let st = node.stat();
    let again = validate_device(&roots, &RealStat, req.dev()).map_err(Validation)?;
    if !st.is_block || st.is_symlink || (st.major, st.minor) != (valid.major, valid.minor) || again != valid {
        return Err(Validation(format!("{} changed during validation", valid.name.path())));
    }
    let grant = node.grant(uid).map_err(|e| Internal(format!("cannot grant device access: {e}")))?;

    let plan = ExecPlan {
        program: exec::FLASHCP.into(),
        args: req.flashcp_args(),
        env: exec::user_env(&creds.home),
        drop: Some(creds),
    };
    let status = exec::run(&plan, Stdio::inherit());
    drop(grant);
    let status = status.map_err(|e| Internal(format!("cannot run {}: {e}", exec::FLASHCP)))?;
    Ok(exec::exit_code(status))
}
