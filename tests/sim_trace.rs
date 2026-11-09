//! The device-op trace recorder (FLASHCP_TRACE / --trace) and the simulated-device override
//! (FLASHCP_SIM_DEVICE), end to end on image files:
//! - tracing changes nothing: the same build traced and untraced writes identical volumes,
//! - recordings are deterministic (same ops, same order within the dependency rule),
//! - an image recorded "as the SanDisk" gets the SanDisk's strategy (BOT pull: gap 0, 2 in
//!   flight, 120 KB reads, 512 B tails; 4 KB cluster cap; partition offset), and nothing is
//!   written to the device-profile cache.

#[allow(dead_code)]
mod common;

use common::{assert_success, flashcp, make_image, test_dir};
use std::fs;
use std::path::Path;

const SANDISK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/sim/devices/sandisk.dev");

fn gen_tiny(dir: &Path) {
    let out = flashcp().arg("gen").arg(dir).args(["--profile", "tiny"]).output().unwrap();
    assert_success(&out, "flashcp gen");
}

/// The io records of a trace as (op, off, len, qd), each run of one parallel group sorted by
/// seq (the order the issuing code fixed; racing threads may write the lines out of order).
/// (op, off, len, qd) of one io record.
type Io = (String, u64, u64, u64);

fn ios(trace: &str) -> Vec<Io> {
    let field = |l: &str, k: &str| -> String {
        let i = l.find(&format!("\"{k}\":")).unwrap_or_else(|| panic!("no {k} in {l}")) + k.len() + 3;
        l[i..].chars().take_while(|c| *c != ',' && *c != '}').collect::<String>().trim_matches('"').to_string()
    };
    let mut v: Vec<(u64, u64, Io)> = Vec::new();
    for l in trace.lines().filter(|l| l.contains(r#""ev":"io""#)) {
        let n = |k| field(l, k).parse::<u64>().unwrap();
        v.push((n("grp"), n("seq"), (field(l, "op"), n("off"), n("len"), n("qd"))));
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < v.len() {
        let mut j = i + 1;
        while v[i].0 != 0 && j < v.len() && v[j].0 == v[i].0 {
            j += 1;
        }
        let mut run = v[i..j].to_vec();
        run.sort_by_key(|r| r.1);
        out.extend(run.into_iter().map(|r| r.2));
        i = j;
    }
    out
}

#[test]
fn tracing_off_and_on_write_identical_volumes() {
    let d = test_dir("sim_trace_identical");
    let src = d.join("src");
    gen_tiny(&src);
    let (a, b, tr) = (d.join("a.img"), d.join("b.img"), d.join("t.jsonl"));
    for img in [&a, &b] {
        make_image(img, 64 << 20);
    }
    // The volume serial comes from the clock, so it is pinned by comparing past the boot regions.
    assert_success(&flashcp().arg("build").arg(&src).arg(&a).arg("--yes-erase").output().unwrap(), "build");
    let out = flashcp().arg("--trace").arg(&tr).arg("build").arg(&src).arg(&b).arg("--yes-erase").output().unwrap();
    assert_success(&out, "traced build");
    let (va, vb) = (fs::read(&a).unwrap(), fs::read(&b).unwrap());
    let skip = 24 * 512;
    assert!(va[skip..] == vb[skip..], "traced and untraced builds differ");
    // The boot regions differ in the serial and the checksum sector (the checksum repeated
    // 128 times) of each of the two regions only.
    let diff = va[..skip].iter().zip(&vb[..skip]).filter(|(x, y)| x != y).count();
    assert!(diff <= 2 * (4 + 512), "boot regions differ in {diff} bytes");
    let t = fs::read_to_string(&tr).unwrap();
    assert!(t.lines().next().unwrap().starts_with(r#"{"v":1,"ev":"start","cmd":"build""#), "{t}");
    assert!(t.lines().last().unwrap().starts_with(r#"{"ev":"end""#));
    let w: u64 = ios(&t).iter().filter(|o| o.0 == "w").map(|o| o.2).sum();
    assert!(w >= 4 << 20, "build wrote only {w} traced bytes");
    assert!(t.contains(r#""ev":"sync""#));
}

#[test]
fn recordings_are_deterministic_and_follow_the_sim_device() {
    let d = test_dir("sim_trace_sandisk");
    let src = d.join("src");
    gen_tiny(&src);
    let img = d.join("v.img");
    make_image(&img, 256 << 20);
    let cache = d.join("cache");
    let sim = |c: &mut std::process::Command| {
        c.env("FLASHCP_SIM_DEVICE", SANDISK).env("XDG_CACHE_HOME", &cache);
    };
    let mut b = flashcp();
    b.arg("build").arg(&src).arg(&img).arg("--yes-erase");
    sim(&mut b);
    let out = b.output().unwrap();
    assert_success(&out, "build as sandisk");
    let plan = String::from_utf8_lossy(&out.stdout);
    assert!(plan.contains("≤ 4 KB on a small removable drive"), "{plan}");
    // PartitionOffset (boot sector bytes 64..72) carries the simulated partition start.
    let v = fs::read(&img).unwrap();
    assert_eq!(u64::from_le_bytes(v[64..72].try_into().unwrap()), 2048);

    let mut traces = Vec::new();
    for k in 0..2 {
        let tr = d.join(format!("pull{k}.jsonl"));
        let mut p = flashcp();
        p.arg("pull").arg(&img).arg(d.join(format!("dest{k}"))).env("FLASHCP_TRACE", &tr);
        sim(&mut p);
        assert_success(&p.output().unwrap(), "pull as sandisk");
        traces.push(fs::read_to_string(&tr).unwrap());
    }
    let t = &traces[0];
    assert!(t.lines().next().unwrap().contains(r#""device":"sandisk-cruzer-usb2""#), "{t}");
    assert!(
        t.contains(r#""transport":"usb-storage","gap_kb":0,"inflight":2"#)
            && t.contains(r#""chunk_kb":120,"tail":512"#),
        "strategy note missing or wrong"
    );
    let (a, b) = (ios(&traces[0]), ios(&traces[1]));
    // The data reads (the parallel group) are the same sequence. The metadata walk's
    // directory reads are the same set, but their order varies from process to process
    // (flashcp itself does that, it is not the recorder), so they are compared sorted.
    let split = |v: &[Io]| {
        let mut meta: Vec<_> = v.iter().filter(|o| o.3 == 1).cloned().collect();
        meta.sort();
        (meta, v.iter().filter(|o| o.3 > 1).cloned().collect::<Vec<_>>())
    };
    assert_eq!(split(&a), split(&b), "two recordings of the same pull differ");
    let reads: Vec<_> = a.iter().filter(|o| o.0 == "r" && o.3 == 2).collect();
    assert!(!reads.is_empty() && reads.iter().all(|o| o.2 <= 120 << 10), "data reads must be ≤ 120 KB, 2 in flight");
    // Nothing was cached for the simulated device.
    assert!(!cache.join("flashcp/devices").exists());
}

#[test]
fn bad_sim_device_file_is_an_error() {
    let d = test_dir("sim_trace_bad");
    let bad = d.join("bad.dev");
    fs::write(&bad, "not a profile\n").unwrap();
    let img = d.join("v.img");
    make_image(&img, 64 << 20);
    let src = d.join("src");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("f"), "x").unwrap();
    let out =
        flashcp().arg("build").arg(&src).arg(&img).arg("--yes-erase").env("FLASHCP_SIM_DEVICE", &bad).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a flashcp-devprofile file"));
}

#[test]
fn sim_run_replays_a_trace_without_touching_it() {
    let d = test_dir("sim_trace_replay");
    let src = d.join("src");
    gen_tiny(&src);
    let (img, tr) = (d.join("v.img"), d.join("t.jsonl"));
    make_image(&img, 64 << 20);
    let out = flashcp()
        .arg("build")
        .arg(&src)
        .arg(&img)
        .arg("--yes-erase")
        .arg("--trace")
        .arg(&tr)
        .env("FLASHCP_SIM_DEVICE", SANDISK)
        .output()
        .unwrap();
    assert_success(&out, "traced build");
    let before = fs::read(&tr).unwrap();
    // Also with FLASHCP_TRACE pointing at the trace: `sim` is never traced itself.
    let out = flashcp()
        .args(["sim", "run"])
        .arg(&tr)
        .args(["--profile", "sandisk", "--profile", "msi", "--json"])
        .env("FLASHCP_TRACE", &tr)
        .output()
        .unwrap();
    assert_success(&out, "sim run");
    assert_eq!(fs::read(&tr).unwrap(), before, "sim run modified its input trace");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(r#""profile":"sandisk""#) && s.contains(r#""profile":"msi""#), "{s}");
}
