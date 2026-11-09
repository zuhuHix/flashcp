//! "Copy from USB": pure rules for the pull feature (folder names on the drive, destinations,
//! which drive a path is on, default Downloads folder, folder listing and destination status).
//! The Tauri commands in lib.rs and the job in runner.rs/job.rs use these.

use crate::drives::Drive;
use serde::Serialize;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Longest folder name on the drive, in bytes. Same limit as the helper's check_from.
const MAX_FROM: usize = 1024;
const NOT_ON_DRIVE: &str = "That folder is not on a USB exFAT drive flashcp can see";
const FAKE_TREE_ROOT: &[&str] = &["DCIM", "Documents", "Music"];

/// What `pull` does when files already exist in the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Existing {
    /// Neither flag: flashcp refuses to start when anything would collide.
    Fail,
    Skip,
    Overwrite,
}

impl Existing {
    /// Parses the IPC value: "fail", "skip" or "overwrite".
    pub fn parse(value: &str) -> Result<Existing, String> {
        match value {
            "fail" => Ok(Existing::Fail),
            "skip" => Ok(Existing::Skip),
            "overwrite" => Ok(Existing::Overwrite),
            other => Err(format!("Unknown choice for existing files: {other}")),
        }
    }

    /// The flashcp/helper flag for this choice, if any.
    pub fn flag(self) -> Option<&'static str> {
        match self {
            Existing::Fail => None,
            Existing::Skip => Some("--skip-existing"),
            Existing::Overwrite => Some("--overwrite"),
        }
    }
}

/// Folder on the drive, relative, from the IPC `from` value. Surrounding '/' are trimmed first.
/// Empty means the whole drive. Anything else must pass the helper's check_from rule: non-empty
/// components, none "." or "..", no leading '-', no control characters, at most 1024 bytes.
pub fn normalize_from(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let refused = trimmed.len() > MAX_FROM
        || trimmed.starts_with('-')
        || trimmed.chars().any(char::is_control)
        || trimmed.split('/').any(|c| c.is_empty() || c == "." || c == "..");
    if refused {
        return Err(format!("Folder on the drive is not allowed: {raw}"));
    }
    Ok(trimmed.to_string())
}

/// The local destination: an absolute path with no NUL byte.
pub fn local_dest(dest: &str) -> Result<String, String> {
    if !Path::new(dest).is_absolute() || dest.contains('\0') {
        return Err(format!("The destination must be an absolute folder path: {dest}"));
    }
    Ok(dest.to_string())
}

/// True when the destination is inside the drive's mount point (component-wise). Copying onto
/// the drive being read would be refused or lost when it unmounts, so the job never starts.
pub fn is_on_drive(dest: &str, drive: &Drive) -> bool {
    drive.mount_point.as_deref().is_some_and(|mp| Path::new(dest).starts_with(mp))
}

/// flashcp's collision message (see flashcp's pull). The GUI shows the skip/overwrite choice for it.
pub fn is_collision(message: &str) -> bool {
    message.contains("already exist in the destination")
}

/// Where a mounted path lives: the drive and the folder on it ("" at the mount root).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub drive_id: String,
    pub from: String,
}

/// Finds the drive whose mount point is the longest component-wise ancestor of `canonical`.
/// `/media/u/STICK2/x` is not under `/media/u/STICK`. The drive must be exFAT.
pub fn resolve_path(drives: &[Drive], canonical: &Path) -> Result<Target, String> {
    let mut best: Option<(&Drive, &Path)> = None;
    for drive in drives {
        let Some(mount) = drive.mount_point.as_deref().map(Path::new) else { continue };
        if !canonical.starts_with(mount) {
            continue;
        }
        let deeper = best.is_none_or(|(_, b)| mount.components().count() > b.components().count());
        if deeper {
            best = Some((drive, mount));
        }
    }
    let (drive, mount) = best.ok_or(NOT_ON_DRIVE)?;
    if !drive.is_exfat {
        return Err(NOT_ON_DRIVE.to_string());
    }
    let rest = canonical.strip_prefix(mount).map_err(|_| NOT_ON_DRIVE.to_string())?;
    let parts = rest
        .components()
        .map(|c| c.as_os_str().to_str().ok_or_else(|| NOT_ON_DRIVE.to_string()))
        .collect::<Result<Vec<&str>, String>>()?;
    let from = normalize_from(&parts.join("/"))?;
    Ok(Target { drive_id: drive.id.clone(), from })
}

/// Defaults for the "Copy to" folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PullDefaults {
    pub downloads: String,
}

/// The pending pull request handed to the window (`--pull <path>` or a second instance).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub drive_id: Option<String>,
    pub from: String,
    pub error: Option<String>,
}

fn refused(message: &str) -> PullRequest {
    PullRequest { drive_id: None, from: String::new(), error: Some(message.to_string()) }
}

/// Resolves a folder path from a file manager into a PullRequest. Never fails: a problem is
/// reported in `error`, so the window can show it.
pub fn request_for_path(drives: &[Drive], path: &Path) -> PullRequest {
    let Ok(canonical) = fs::canonicalize(path) else {
        return refused(NOT_ON_DRIVE);
    };
    match resolve_path(drives, &canonical) {
        Ok(t) => PullRequest { drive_id: Some(t.drive_id), from: t.from, error: None },
        Err(e) => refused(&e),
    }
}

/// Sub-directory names of `dir`, skipping symlinks and names starting with '.', sorted
/// case-insensitively.
pub fn list_subdirs(dir: &Path) -> Result<Vec<String>, String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("Cannot read {}: {e}", dir.display()))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("Cannot read {}: {e}", dir.display()))?;
        let Ok(name) = entry.file_name().into_string() else { continue };
        if name.starts_with('.') {
            continue;
        }
        // DirEntry::file_type does not follow symlinks, so links are skipped.
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            names.push(name);
        }
    }
    names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b)));
    Ok(names)
}

/// The fixed folder tree shown in dev mode (FLASHCP_GUI_FAKE_DRIVES=1). `rel` is already normalized.
pub fn fake_folders(rel: &str) -> Vec<String> {
    let names: &[&str] = match rel {
        "" => FAKE_TREE_ROOT,
        "DCIM" => &["100CANON", "101CANON"],
        "Documents" => &["2026"],
        _ => &[],
    };
    names.iter().map(|s| s.to_string()).collect()
}

/// What the destination looks like on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DestStatus {
    pub exists: bool,
    pub is_dir: bool,
    pub empty: bool,
}

/// Status of an absolute path, as the current user sees it. A missing path is "not there, empty".
pub fn dest_status(path: &Path) -> Result<DestStatus, String> {
    if !path.is_absolute() {
        return Err(format!("Not an absolute path: {}", path.display()));
    }
    match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => {
            let empty =
                fs::read_dir(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?.next().is_none();
            Ok(DestStatus { exists: true, is_dir: true, empty })
        }
        Ok(_) => Ok(DestStatus { exists: true, is_dir: false, empty: false }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(DestStatus { exists: false, is_dir: false, empty: true }),
        Err(e) => Err(format!("Cannot read {}: {e}", path.display())),
    }
}

/// Default destination root: XDG_DOWNLOAD_DIR from user-dirs.dirs when it is absolute, else
/// $HOME/Downloads. `user_dirs` is the file's text, `home` is $HOME.
pub fn downloads_from(user_dirs: Option<&str>, home: Option<&str>) -> Result<String, String> {
    if let Some(dir) = user_dirs.and_then(|text| xdg_download_dir(text, home)) {
        return Ok(dir);
    }
    let home = home.ok_or("neither HOME is set")?;
    Ok(PathBuf::from(home).join("Downloads").to_string_lossy().into_owned())
}

/// The absolute XDG_DOWNLOAD_DIR in `text` (`XDG_DOWNLOAD_DIR="$HOME/Downloads"`), with $HOME or
/// ${HOME} at the start expanded. None when the key is missing, relative, or HOME is needed and absent.
fn xdg_download_dir(text: &str, home: Option<&str>) -> Option<String> {
    let line = text.lines().map(str::trim).find_map(|l| l.strip_prefix("XDG_DOWNLOAD_DIR="))?;
    let value = line.trim();
    let value = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value);
    let expanded = match value.strip_prefix("$HOME").or_else(|| value.strip_prefix("${HOME}")) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("{}{rest}", home?),
        Some(_) => return None,
        None => value.to_string(),
    };
    Path::new(&expanded).is_absolute().then_some(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drives::TrustIdentity;

    fn drive(id: &str, mount: Option<&str>, exfat: bool) -> Drive {
        Drive {
            id: id.into(),
            device: format!("/dev/{id}"),
            label: "X".into(),
            fs_type: if exfat { "exfat".into() } else { "vfat".into() },
            is_exfat: exfat,
            size: 1,
            mount_point: mount.map(str::to_string),
            model: "M".into(),
            whole_disk: false,
            serial: "S".into(),
            identity: TrustIdentity::from_udisks("S", "", "M", 1),
            trusted: true,
        }
    }

    #[test]
    fn from_accepts_canonical_relative_folders() {
        for ok in ["Photos", "Photos/2026", "a.b/c-d_e", "..x"] {
            assert_eq!(normalize_from(ok).unwrap(), ok, "{ok:?}");
        }
        assert_eq!(normalize_from("").unwrap(), "");
        assert_eq!(normalize_from("/").unwrap(), "");
        assert_eq!(normalize_from("/Photos/2026/").unwrap(), "Photos/2026");
    }

    #[test]
    fn from_refuses_what_the_helper_refuses() {
        let long = "a".repeat(MAX_FROM + 1);
        for bad in ["a//b", "a/./b", "a/../b", "../x", "x/..", "-x", "/-x", "a\nb", "a\u{7}", long.as_str()] {
            let err = normalize_from(bad).expect_err(bad);
            assert!(err.starts_with("Folder on the drive is not allowed: "), "{err}");
        }
        assert!(normalize_from(&"a".repeat(MAX_FROM)).is_ok());
    }

    #[test]
    fn dest_must_be_absolute_without_nul() {
        assert_eq!(local_dest("/home/u/Downloads/USB").unwrap(), "/home/u/Downloads/USB");
        assert!(local_dest("relative/dir").is_err());
        assert!(local_dest("").is_err());
        assert!(local_dest("/a\0b").is_err());
    }

    #[test]
    fn destination_on_the_drive_is_detected() {
        let stick = drive("sdz1", Some("/media/u/STICK"), true);
        assert!(is_on_drive("/media/u/STICK", &stick));
        assert!(is_on_drive("/media/u/STICK/out", &stick));
        assert!(!is_on_drive("/media/u/STICK2/out", &stick));
        assert!(!is_on_drive("/home/u/out", &stick));
        assert!(!is_on_drive("/media/u/STICK", &drive("x", None, true)));
    }

    #[test]
    fn collision_message_is_detected() {
        assert!(is_collision(
            "3 file(s) already exist in the destination /d, e.g. a; pass --skip-existing or --overwrite"
        ));
        assert!(!is_collision("Unmount failed"));
    }

    #[test]
    fn existing_choices_parse() {
        assert_eq!(Existing::parse("fail").unwrap(), Existing::Fail);
        assert_eq!(Existing::parse("skip").unwrap(), Existing::Skip);
        assert_eq!(Existing::parse("overwrite").unwrap(), Existing::Overwrite);
        assert!(Existing::parse("Skip").is_err());
        assert_eq!(Existing::Fail.flag(), None);
        assert_eq!(Existing::Skip.flag(), Some("--skip-existing"));
        assert_eq!(Existing::Overwrite.flag(), Some("--overwrite"));
    }

    #[test]
    fn resolves_the_longest_component_wise_ancestor() {
        let drives = [drive("sda1", Some("/media/u/STICK"), true), drive("sdb1", Some("/media/u/STICK2"), true)];
        let got = resolve_path(&drives, Path::new("/media/u/STICK2/Photos/2026")).unwrap();
        assert_eq!(got, Target { drive_id: "sdb1".into(), from: "Photos/2026".into() });
        let got = resolve_path(&drives, Path::new("/media/u/STICK")).unwrap();
        assert_eq!(got, Target { drive_id: "sda1".into(), from: String::new() });
    }

    #[test]
    fn nested_mounts_pick_the_deepest() {
        let drives = [drive("outer", Some("/media/u"), true), drive("inner", Some("/media/u/STICK"), true)];
        let got = resolve_path(&drives, Path::new("/media/u/STICK/x")).unwrap();
        assert_eq!(got.drive_id, "inner");
        assert_eq!(got.from, "x");
    }

    #[test]
    fn paths_off_drives_and_non_exfat_are_refused() {
        let drives = [drive("sda1", Some("/media/u/STICK"), true), drive("sdy1", Some("/media/u/OLD"), false)];
        let msg = "That folder is not on a USB exFAT drive flashcp can see";
        assert_eq!(resolve_path(&drives, Path::new("/home/u/x")).unwrap_err(), msg);
        assert_eq!(resolve_path(&drives, Path::new("/media/u/STICK2")).unwrap_err(), msg);
        assert_eq!(resolve_path(&drives, Path::new("/media/u/OLD/x")).unwrap_err(), msg);
        assert_eq!(resolve_path(&[drive("n", None, true)], Path::new("/media/u/x")).unwrap_err(), msg);
    }

    #[test]
    fn request_for_a_real_folder_on_a_fake_drive() {
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("STICK");
        fs::create_dir_all(mount.join("Photos/2026")).unwrap();
        let drives = [drive("sda1", Some(mount.to_str().unwrap()), true)];

        let got = request_for_path(&drives, &mount.join("Photos/2026"));
        assert_eq!(got, PullRequest { drive_id: Some("sda1".into()), from: "Photos/2026".into(), error: None });

        let missing = request_for_path(&drives, &mount.join("nope"));
        assert_eq!(missing.drive_id, None);
        assert!(missing.error.is_some());
    }

    #[test]
    fn subdirs_skip_files_hidden_and_symlinks_and_sort_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for d in ["beta", "Alpha", "gamma", ".hidden"] {
            fs::create_dir(root.join(d)).unwrap();
        }
        fs::write(root.join("file.txt"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("beta"), root.join("link")).unwrap();
        assert_eq!(list_subdirs(root).unwrap(), vec!["Alpha", "beta", "gamma"]);
        assert!(list_subdirs(&root.join("missing")).is_err());
    }

    #[test]
    fn fake_tree_is_fixed() {
        assert_eq!(fake_folders(""), vec!["DCIM", "Documents", "Music"]);
        assert_eq!(fake_folders("DCIM"), vec!["100CANON", "101CANON"]);
        assert_eq!(fake_folders("Documents"), vec!["2026"]);
        assert!(fake_folders("Music").is_empty());
    }

    #[test]
    fn dest_status_reports_missing_empty_and_full() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert_eq!(dest_status(&missing).unwrap(), DestStatus { exists: false, is_dir: false, empty: true });

        let empty = dir.path().join("empty");
        fs::create_dir(&empty).unwrap();
        assert_eq!(dest_status(&empty).unwrap(), DestStatus { exists: true, is_dir: true, empty: true });

        fs::write(empty.join("f"), "x").unwrap();
        assert_eq!(dest_status(&empty).unwrap(), DestStatus { exists: true, is_dir: true, empty: false });

        let file = dir.path().join("f");
        fs::write(&file, "x").unwrap();
        assert_eq!(dest_status(&file).unwrap(), DestStatus { exists: true, is_dir: false, empty: false });

        assert!(dest_status(Path::new("relative")).is_err());
    }

    #[test]
    fn downloads_default_follows_user_dirs() {
        let home = Some("/home/u");
        let plain = "# comment\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOWNLOAD_DIR=\"$HOME/Downloads\"\n";
        assert_eq!(downloads_from(Some(plain), home).unwrap(), "/home/u/Downloads");
        assert_eq!(downloads_from(Some("XDG_DOWNLOAD_DIR=\"${HOME}/Dl\"\n"), home).unwrap(), "/home/u/Dl");
        assert_eq!(downloads_from(Some("XDG_DOWNLOAD_DIR=\"/data/dl\"\n"), home).unwrap(), "/data/dl");
        assert_eq!(downloads_from(Some("XDG_DOWNLOAD_DIR=$HOME\n"), home).unwrap(), "/home/u");
        // relative values and missing keys fall back to $HOME/Downloads
        assert_eq!(downloads_from(Some("XDG_DOWNLOAD_DIR=\"Downloads\"\n"), home).unwrap(), "/home/u/Downloads");
        assert_eq!(downloads_from(Some("XDG_MUSIC_DIR=\"$HOME/M\"\n"), home).unwrap(), "/home/u/Downloads");
        assert_eq!(downloads_from(None, home).unwrap(), "/home/u/Downloads");
        // $HOME with no HOME, and no HOME at all
        assert!(downloads_from(Some("XDG_DOWNLOAD_DIR=\"$HOME/Dl\"\n"), None).is_err());
        assert!(downloads_from(None, None).is_err());
    }
}
