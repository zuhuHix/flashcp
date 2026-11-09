//! `add` onto an EMPTY volume with coarse clusters (a fresh mkfs/Windows stick, typically 32 KB)
//! is planned as a `build` instead: the whole volume is written in one sequential pass with the
//! optimizer's cluster size, and the label is kept. Only when the volume provably holds nothing:
//! no entries in the root directory besides the label / bitmap / up-case entries, and no
//! allocated clusters besides the bitmap, the up-case table and the root directory itself.
//! A volume with any user data always goes through the normal, non-destructive add.

use crate::add::AddStats;
use crate::events::{Event, Op, Sink};
use crate::exfat::{
    BuildOpts, FatMode, Node, Target, build_target, choose_cluster, scan_tree, set_direct, validate_name,
};
use crate::volume::Vol;
use anyhow::{Context, Result, ensure};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Test hook: never turn an add into a build.
const NO_BUILD_ENV: &str = "FLASHCP_TEST_NO_ADD_BUILD";

/// Whether the parsed volume holds no user data at all.
fn is_empty(v: &Vol) -> bool {
    let Some(root) = v.dirs.first() else { return false };
    let system = (v.bitmap_clusters.len() + v.upcase_clusters.len() + root.clusters.len()) as u64;
    root.children.is_empty() && v.cc - v.free_clusters() == system
}

/// Puts the scanned tree under the `--to` path ("a/b"): new root → a → b, where b is the old root.
fn wrap(mut nodes: Vec<Node>, to: &str, upc: &[u16], now: (i64, u32)) -> Result<Vec<Node>> {
    let comps: Vec<&str> = to.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
    if comps.is_empty() {
        return Ok(nodes);
    }
    let k = comps.len();
    for c in &comps {
        ensure!(*c != "..", "--to must not contain '..'");
        validate_name(&c.encode_utf16().collect::<Vec<_>>(), upc, &mut HashSet::new(), Path::new(c))?;
    }
    for n in &mut nodes {
        for c in &mut n.children {
            *c += k;
        }
    }
    let dir = |name: &str, child: usize| Node {
        name: name.encode_utf16().collect(),
        src: PathBuf::new(),
        is_dir: true,
        size: 0,
        mtime: now,
        children: vec![child],
        first_cluster: 0,
        clusters: 0,
    };
    // Index 0 is the new root, 1..k-1 the intermediate dirs, k the old root (renamed).
    let mut out = vec![dir("", 1)];
    for (i, c) in comps[..k - 1].iter().enumerate() {
        out.push(dir(c, i + 2));
    }
    nodes[0].name = comps[k - 1].encode_utf16().collect();
    nodes[0].mtime = now;
    out.extend(nodes);
    Ok(out)
}

/// Builds `src` onto `tgt` when the volume there is empty and the optimizer would pick a smaller
/// cluster size than it has. Returns the target back (untouched) when the normal add applies.
pub(crate) fn empty_as_build(
    src: &Path,
    tgt: Target,
    to: Option<&str>,
    readers: usize,
    dry_run: bool,
    sink: Sink<'_>,
) -> Result<std::result::Result<AddStats, Target>> {
    if std::env::var_os(NO_BUILD_ENV).is_some() {
        return Ok(Err(tgt));
    }
    set_direct(&tgt.file, false);
    let v = Vol::open(tgt.file.try_clone().context("dup target fd")?, tgt.bytes, false, true)?;
    if !is_empty(&v) {
        return Ok(Err(tgt));
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
    let mut nodes = scan_tree(src, &v.upc)?;
    if let Some(to) = to {
        nodes = wrap(nodes, to, &v.upc, (now.as_secs() as i64, now.subsec_nanos()))?;
    }
    let bo = BuildOpts {
        cluster: None,
        label: v.label.clone(),
        fat: FatMode::Prefix,
        readers: readers.max(1),
        dry_run,
        serial: None,
        job: None,
    };
    let (p, _) = choose_cluster(&mut nodes, &tgt, &bo)?;
    if p.g.cluster >= v.cs {
        return Ok(Err(tgt));
    }
    let old = v.cs;
    drop(v);
    // Report it as the add the caller asked for.
    let mut fwd = |e: &Event| match e {
        Event::Plan { files, dirs, bytes, write_bytes, detail, strategy, .. } => sink(&Event::Plan {
            op: Op::Add,
            files: *files,
            dirs: *dirs,
            bytes: *bytes,
            write_bytes: *write_bytes,
            detail: format!(
                "the volume is empty: writing it as one sequential build (cluster {} KB → smaller, label kept) │ {detail}",
                old >> 10
            ),
            strategy: strategy.clone(),
            sync: None,
        }),
        e => sink(e),
    };
    let s = build_target(nodes, tgt, &bo, &mut fwd)?;
    Ok(Ok(AddStats {
        files: s.files as u64,
        // The --to directories are new too; the build counts them with the root's own entry.
        dirs: s.dirs.saturating_sub(1) as u64,
        skipped: 0,
        data_bytes: s.data_bytes,
        written: s.written,
        fragmented: 0,
        cluster: s.cluster,
        secs: s.secs,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(name: &str, dir: bool, children: Vec<usize>) -> Node {
        Node {
            name: name.encode_utf16().collect(),
            src: PathBuf::new(),
            is_dir: dir,
            size: 0,
            mtime: (0, 0),
            children,
            first_cluster: 0,
            clusters: 0,
        }
    }

    #[test]
    fn wrap_moves_the_tree_under_the_path() {
        let upc = crate::exfat::upcase_map();
        let nodes = vec![n("", true, vec![1, 2]), n("f", false, vec![]), n("d", true, vec![3]), n("g", false, vec![])];
        let w = wrap(nodes, "a/b", &upc, (1, 0)).unwrap();
        assert_eq!(w.len(), 6);
        assert_eq!(w[0].children, vec![1]);
        assert_eq!(String::from_utf16_lossy(&w[1].name), "a");
        assert_eq!(w[1].children, vec![2]);
        assert_eq!(String::from_utf16_lossy(&w[2].name), "b");
        assert_eq!(w[2].children, vec![3, 4]);
        assert_eq!(w[4].children, vec![5]);
        assert!(wrap(vec![n("", true, vec![])], "x/../y", &upc, (0, 0)).is_err());
    }
}
