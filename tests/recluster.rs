//! The command-line contract of `flashcp recluster` that the daemon and the app build on: exit
//! codes and JSON lines of `--check`, `--dry-run`, a run and a refusal, on a volume the real
//! binary built. The result is read back with tests/common/exfat_reader.rs (a parser written
//! from the exFAT spec that shares no code with flashcp) and with `fsck.exfat -n`.
//!
//! What the conversion itself must hold (random trees and geometries, a crash at every device
//! write, every refusal) is tested in process, in src/recluster/tests.rs.

#[allow(dead_code)]
mod common;

use common::{MB, assert_success, compare_trees, exfat_reader, flashcp, fsck_clean, make_image, make_tricky_tree};
use std::path::Path;
use std::process::Output;

fn recluster(img: &Path, args: &[&str]) -> Output {
    flashcp().arg("recluster").arg(img).args(args).output().expect("run flashcp recluster")
}

fn lines(out: &Output) -> Vec<String> {
    let l: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect();
    for x in &l {
        assert!(x.starts_with("{\"event\":\"") && x.ends_with('}'), "not one JSON event per line: {x}");
    }
    l
}

/// Value of a field of a flat JSON object, without quotes.
fn field<'a>(obj: &'a str, key: &str) -> &'a str {
    let pat = format!("\"{key}\":");
    let rest = &obj[obj.find(&pat).unwrap_or_else(|| panic!("no field {key:?} in {obj}")) + pat.len()..];
    rest[..rest.find([',', '}']).unwrap_or(rest.len())].trim_matches('"')
}

fn sum(p: &Path) -> u64 {
    use std::hash::Hasher;
    let mut h = std::hash::DefaultHasher::new();
    h.write(&std::fs::read(p).unwrap());
    h.finish()
}

#[test]
fn check_dry_run_run_and_refusal_speak_the_contract() {
    let dir = common::test_dir("recluster_cli");
    let (src, img) = (dir.join("src"), dir.join("vol.img"));
    make_tricky_tree(&src);
    make_image(&img, 96 * MB);
    assert_success(&common::run_build(&src, &img, &["--cluster", "32768"]), "flashcp build");
    let tree = common::read_source_tree(&src);
    let built = sum(&img);

    // --check: yes, exit 0, one line.
    let out = recluster(&img, &["--check", "--json"]);
    let l = lines(&out);
    assert_eq!((out.status.code(), l.len()), (Some(0), 1), "{l:?}");
    assert_eq!(
        (field(&l[0], "event"), field(&l[0], "benefit"), field(&l[0], "reason"), field(&l[0], "finish")),
        ("check", "true", "ok", "false")
    );
    assert_eq!((field(&l[0], "cluster"), field(&l[0], "new_cluster")), ("32768", "4096"));

    // --expect-serial: another volume's serial is refused before anything is written.
    let out = recluster(&img, &["--json", "--expect-serial", "0BAD-F00D"]);
    let l = lines(&out);
    assert_eq!(out.status.code(), Some(3), "{l:?}");
    assert_eq!((field(&l[0], "event"), field(&l[0], "reason")), ("refused", "wrong-volume"));
    assert_eq!(sum(&img), built, "a refused --expect-serial wrote to the image");

    // --dry-run: the plan and nothing else, nothing written.
    let out = recluster(&img, &["--dry-run", "--json"]);
    let l = lines(&out);
    assert_eq!((out.status.code(), l.len()), (Some(0), 1), "{l:?}");
    assert_eq!((field(&l[0], "event"), field(&l[0], "op"), field(&l[0], "new_cluster")), ("plan", "recluster", "4096"));
    let planned: u64 = field(&l[0], "write_bytes").parse().unwrap();
    assert_eq!(sum(&img), built, "--dry-run wrote to the image");

    // The run (with this volume's own serial): plan, the switches, a last progress at 100 %, done.
    let boot = std::fs::read(&img).unwrap();
    let serial = u32::from_le_bytes([boot[100], boot[101], boot[102], boot[103]]);
    let own = format!("{:04X}-{:04X}", serial >> 16, serial & 0xffff);
    let out = recluster(&img, &["--json", "--expect-serial", &own]);
    let l = lines(&out);
    assert_eq!(out.status.code(), Some(0), "{l:?}\n{}", String::from_utf8_lossy(&out.stderr));
    let events: Vec<&str> = l.iter().map(|x| field(x, "event")).filter(|e| *e != "progress").collect();
    let switches: usize = field(&l[0], "switches").parse().unwrap();
    assert_eq!(events, [&["plan"][..], &vec!["switched"; switches], &["done"]].concat(), "{l:?}");
    let steps: Vec<&str> = l.iter().filter(|x| field(x, "event") == "switched").map(|x| field(x, "step")).collect();
    assert_eq!(steps.last(), Some(&"reclustered"), "{l:?}");
    let (last, done) = (&l[l.len() - 2], &l[l.len() - 1]);
    assert_eq!((field(last, "event"), field(last, "bytes_done")), ("progress", field(last, "bytes_total")));
    assert_eq!(
        (field(done, "changed"), field(done, "cluster"), field(done, "old_cluster"), field(done, "written")),
        ("true", "4096", "32768", planned.to_string().as_str())
    );

    // What an independent parser and fsck.exfat make of the result.
    fsck_clean(&img);
    let vol = exfat_reader::read_volume(&img).unwrap_or_else(|e| panic!("read back: {e}"));
    assert_eq!(vol.info.cluster_size, 4096);
    let diffs = compare_trees(&tree, &vol);
    assert!(diffs.is_empty(), "the tree changed:\n{}", diffs.join("\n"));

    // Again: nothing to do (exit 0), and --check says no (exit 3) with the reason.
    let out = recluster(&img, &["--json"]);
    let l = lines(&out);
    assert_eq!((out.status.code(), l.len(), field(&l[0], "changed")), (Some(0), 1, "false"), "{l:?}");
    let out = recluster(&img, &["--check", "--json"]);
    assert_eq!((out.status.code(), field(&lines(&out)[0], "reason")), (Some(3), "small-clusters"));

    // A refusal: exit 3, a `refused` line with the reason, nothing written.
    let (out, before) = (recluster(&img, &["--cluster", "8192", "--json"]), sum(&img));
    let l = lines(&out);
    assert_eq!((out.status.code(), l.len()), (Some(3), 1), "{l:?}");
    assert_eq!((field(&l[0], "event"), field(&l[0], "reason")), ("refused", "bad-cluster-size"));
    assert_eq!(sum(&img), before);
    let out = recluster(&img, &["--cluster", "8192"]);
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("Refused: "), "{out:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The stop hook of the crash-window CI volume exists only in a `test-hooks` build: in any
/// other, the variable is ignored and both boot sectors are written.
#[test]
fn the_stop_hook_is_not_in_a_normal_build() {
    if cfg!(feature = "test-hooks") {
        return;
    }
    let dir = common::test_dir("recluster_hook");
    let (src, img) = (dir.join("src"), dir.join("vol.img"));
    make_tricky_tree(&src);
    make_image(&img, 64 * MB);
    assert_success(&common::run_build(&src, &img, &["--cluster", "32768"]), "flashcp build");
    for step in ["relocated", "reclustered"] {
        let out = flashcp()
            .arg("recluster")
            .arg(&img)
            .args(["--cluster", if step == "relocated" { "8192" } else { "4096" }])
            .env("FLASHCP_TEST_RECLUSTER_STOP", step)
            .output()
            .unwrap();
        assert_success(&out, "flashcp recluster with the stop variable set");
        let b = std::fs::read(&img).unwrap();
        assert_eq!(b[..512], b[12 * 512..13 * 512], "{step}: the backup boot sector was not written");
    }
    fsck_clean(&img);
    std::fs::remove_dir_all(&dir).unwrap();
}
