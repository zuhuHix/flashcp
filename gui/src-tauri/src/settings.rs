//! App settings (settings.json under the XDG config dir) and the XDG autostart entry.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const SYSTEM_AUTOSTART: &str = "/etc/xdg/autostart/flashcp-gui.desktop";
const PACKAGED_BIN: &str = "/usr/bin/flashcp-gui";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub autostart: bool,
    pub notify_on_plug: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { autostart: false, notify_on_plug: true }
    }
}

/// Where everything lives. Injected so tests can use a temp dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub settings_file: PathBuf,
    pub user_autostart: PathBuf,
    pub system_autostart: PathBuf,
}

impl Paths {
    /// $XDG_CONFIG_HOME (or ~/.config) based paths.
    pub fn from_env() -> Result<Paths, String> {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .ok_or_else(|| "neither XDG_CONFIG_HOME nor HOME is set".to_string())?;
        Ok(Paths {
            settings_file: config.join("flashcp-gui").join("settings.json"),
            user_autostart: config.join("autostart").join("flashcp-gui.desktop"),
            system_autostart: PathBuf::from(SYSTEM_AUTOSTART),
        })
    }
}

/// Reads "Hidden=" from a desktop entry. Missing key means not hidden.
fn is_hidden(content: &str) -> bool {
    content.lines().any(|l| l.trim() == "Hidden=true")
}

fn autostart_enabled(paths: &Paths) -> bool {
    match fs::read_to_string(&paths.user_autostart) {
        Ok(content) => !is_hidden(&content),
        Err(_) => paths.system_autostart.exists(),
    }
}

/// Current settings. Missing or unreadable files give the defaults.
pub fn load(paths: &Paths) -> Settings {
    let notify_on_plug = fs::read_to_string(&paths.settings_file)
        .ok()
        .and_then(|s| serde_json::from_str::<Settings>(&s).ok())
        .map(|s| s.notify_on_plug)
        .unwrap_or(true);
    Settings { autostart: autostart_enabled(paths), notify_on_plug }
}

/// Exec line for the autostart entry: bare name for the packaged binary, otherwise the
/// absolute path (dev builds). Always starts the tray with --tray.
pub fn exec_line(exe: &Path) -> String {
    if exe == Path::new(PACKAGED_BIN) {
        return "flashcp-gui --tray".to_string();
    }
    let raw = exe.to_string_lossy();
    if raw.chars().any(|c| " \t\"'\\`$>;&|<*?()[]{}~".contains(c)) {
        let escaped: String =
            raw.chars().flat_map(|c| if "\"`$\\".contains(c) { vec!['\\', c] } else { vec![c] }).collect();
        format!("\"{escaped}\" --tray")
    } else {
        format!("{raw} --tray")
    }
}

fn desktop_entry(exec: &str, hidden: bool) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=flashcp tray\n\
         Comment=Notice USB drives and offer fast copies\n\
         Exec={exec}\n\
         Icon=flashcp-gui\n\
         Terminal=false\n\
         NoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n\
         Hidden={hidden}\n"
    )
}

fn write_file(path: &Path, content: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)
}

/// Stores the settings and writes the user autostart entry (Hidden=false when enabled,
/// Hidden=true when disabled, so a system-wide entry cannot re-enable it). Returns the
/// settings as read back from disk.
pub fn save(paths: &Paths, settings: Settings, exe: &Path) -> Result<Settings, String> {
    let exec = exec_line(exe);
    write_file(&paths.user_autostart, &desktop_entry(&exec, !settings.autostart))
        .map_err(|e| format!("Cannot write autostart entry: {e}"))?;
    let json = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
    write_file(&paths.settings_file, &(json + "\n")).map_err(|e| format!("Cannot write settings: {e}"))?;
    Ok(load(paths))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_in(dir: &Path) -> Paths {
        Paths {
            settings_file: dir.join("cfg/flashcp-gui/settings.json"),
            user_autostart: dir.join("cfg/autostart/flashcp-gui.desktop"),
            system_autostart: dir.join("etc/flashcp-gui.desktop"),
        }
    }

    #[test]
    fn defaults_when_nothing_exists() {
        let dir = tempfile::tempdir().unwrap();
        let s = load(&paths_in(dir.path()));
        assert_eq!(s, Settings { autostart: false, notify_on_plug: true });
    }

    #[test]
    fn system_entry_means_autostart_on() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths_in(dir.path());
        fs::create_dir_all(p.system_autostart.parent().unwrap()).unwrap();
        fs::write(&p.system_autostart, "[Desktop Entry]\n").unwrap();
        assert!(load(&p).autostart);
    }

    #[test]
    fn user_entry_overrides_system_entry() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths_in(dir.path());
        fs::create_dir_all(p.system_autostart.parent().unwrap()).unwrap();
        fs::write(&p.system_autostart, "[Desktop Entry]\n").unwrap();
        write_file(&p.user_autostart, &desktop_entry("flashcp-gui --tray", true)).unwrap();
        assert!(!load(&p).autostart);
    }

    #[test]
    fn enable_and_disable_write_hidden_flag() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths_in(dir.path());
        let exe = Path::new(PACKAGED_BIN);

        let on = save(&p, Settings { autostart: true, notify_on_plug: false }, exe).unwrap();
        assert_eq!(on, Settings { autostart: true, notify_on_plug: false });
        let text = fs::read_to_string(&p.user_autostart).unwrap();
        assert!(text.contains("Hidden=false"));
        assert!(text.contains("Exec=flashcp-gui --tray"));
        assert!(text.contains("X-GNOME-Autostart-enabled=true"));

        let off = save(&p, Settings { autostart: false, notify_on_plug: false }, exe).unwrap();
        assert!(!off.autostart);
        let text = fs::read_to_string(&p.user_autostart).unwrap();
        assert!(text.contains("Hidden=true"));
    }

    #[test]
    fn settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths_in(dir.path());
        let exe = Path::new(PACKAGED_BIN);
        save(&p, Settings { autostart: false, notify_on_plug: false }, exe).unwrap();
        assert!(!load(&p).notify_on_plug);
        save(&p, Settings { autostart: false, notify_on_plug: true }, exe).unwrap();
        assert!(load(&p).notify_on_plug);
    }

    #[test]
    fn corrupt_settings_fall_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths_in(dir.path());
        write_file(&p.settings_file, "{not json").unwrap();
        assert!(load(&p).notify_on_plug);
    }

    #[test]
    fn dev_build_uses_absolute_exec() {
        assert_eq!(
            exec_line(Path::new("/home/zuhu/dev/target/debug/flashcp-gui")),
            "/home/zuhu/dev/target/debug/flashcp-gui --tray"
        );
        assert_eq!(exec_line(Path::new(PACKAGED_BIN)), "flashcp-gui --tray");
        assert_eq!(exec_line(Path::new("/opt/my apps/flashcp-gui")), "\"/opt/my apps/flashcp-gui\" --tray");
    }
}
