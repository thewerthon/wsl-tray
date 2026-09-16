//! Minimal configuration file support (`wsltray.ini`).
//!
//! Deliberately not a real INI: no sections, just `key = value` lines, with
//! `#`/`;` comments and blank lines skipped. This is hand-rolled instead of
//! pulling in a crate to keep the "no runtime, minimal footprint" spirit of
//! the rest of the program; parsing happens once at startup, never on the
//! poll path.

use std::path::{Path, PathBuf};

/// Settings read from `wsltray.ini` (or the file passed as `-config`).
/// Unknown keys are ignored, so old config files keep working after new keys
/// are added; missing or unparsable keys keep their default.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Config {
    /// Start the app with Windows (`HKCU\...\Run`).
    pub autostart: bool,
    /// Run Start automatically once, right after the app launches.
    pub autoboot: bool,
    /// Distribution to target for Start/Restart/Shutdown/Explorer/Terminal.
    /// Empty means "whichever distribution WSL picks by default".
    pub distroname: String,
    /// Windows Terminal profile name for the Terminal menu command. Empty
    /// keeps the default `wsl.exe`-based behaviour.
    pub wtprofile: String,
    /// Which tray icon to show: `"color"`, `"mono"`, or anything else
    /// (including empty) for automatic (colour while WSL2 runs, mono while
    /// it does not).
    pub icon: String,
    /// How often, in milliseconds, to check WSL2's state and refresh the
    /// tray icon/tooltip. `0` (or missing) keeps the built-in default (see
    /// `-poll` in the command line help).
    pub refreshms: u32,
}

/// Default config file name, looked up next to the executable.
const DEFAULT_FILE_NAME: &str = "wsltray.ini";

impl Config {
    /// Loads `path`, or the defaults if it does not exist or cannot be read
    /// (the app must keep working without a config file at all).
    pub fn load(path: &Path) -> Config {
        match std::fs::read_to_string(path) {
            Ok(text) => Config::parse(&text),
            Err(_) => Config::default(),
        }
    }

    /// Parses the `key = value` text described in the module docs.
    fn parse(text: &str) -> Config {
        let mut cfg = Config::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim().to_ascii_lowercase().as_str() {
                "autostart" => cfg.autostart = parse_bool(value).unwrap_or(cfg.autostart),
                "autoboot" => cfg.autoboot = parse_bool(value).unwrap_or(cfg.autoboot),
                "distroname" => cfg.distroname = value.to_string(),
                "wtprofile" => cfg.wtprofile = value.to_string(),
                "icon" => cfg.icon = value.to_string(),
                "refreshms" => cfg.refreshms = value.parse().unwrap_or(cfg.refreshms),
                _ => {}
            }
        }
        cfg
    }
}

/// Accepts `true`/`false`, `1`/`0`, `yes`/`no`, `on`/`off`, case-insensitively.
/// `None` for anything else, so the caller can keep the previous value.
fn parse_bool(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Default config file path: `wsltray.ini` next to the running executable,
/// falling back to the bare file name (current directory) if the executable
/// path cannot be resolved.
pub fn default_path() -> PathBuf {
    let mut dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    dir.push(DEFAULT_FILE_NAME);
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_example() {
        let cfg = Config::parse(
            "autostart = false\nautoboot = false\ndistroname = Ubuntu\nwtprofile =\nicon = auto\nrefreshms = 5000\n",
        );
        assert_eq!(
            cfg,
            Config {
                autostart: false,
                autoboot: false,
                distroname: "Ubuntu".into(),
                wtprofile: String::new(),
                icon: "auto".into(),
                refreshms: 5000,
            }
        );
    }

    #[test]
    fn defaults_are_all_off() {
        assert_eq!(Config::parse(""), Config::default());
        assert!(!Config::default().autostart);
        assert!(!Config::default().autoboot);
        assert_eq!(Config::default().distroname, "");
        assert_eq!(Config::default().wtprofile, "");
        assert_eq!(Config::default().icon, "");
        assert_eq!(Config::default().refreshms, 0);
    }

    #[test]
    fn ignores_comments_blank_lines_and_unknown_keys() {
        let cfg = Config::parse(
            "# comment\n; also a comment\n\nautostart = true\nsome_future_key = 42\n",
        );
        assert!(cfg.autostart);
    }

    #[test]
    fn is_forgiving_about_case_and_spacing() {
        let cfg = Config::parse(
            "  AUTOSTART=TRUE  \nAUTOBOOT=TRUE\nDistroName = Ubuntu\nWtProfile = Ubuntu-dev\nIcon = Mono\nRefreshMs = 2000\n",
        );
        assert!(cfg.autostart);
        assert!(cfg.autoboot);
        assert_eq!(cfg.distroname, "Ubuntu");
        assert_eq!(cfg.wtprofile, "Ubuntu-dev");
        assert_eq!(cfg.icon, "Mono");
        assert_eq!(cfg.refreshms, 2000);
    }

    #[test]
    fn keeps_default_on_bad_bool_value() {
        let cfg = Config::parse("autostart = maybe\nautoboot = maybe\n");
        assert!(!cfg.autostart);
        assert!(!cfg.autoboot);
    }

    #[test]
    fn keeps_default_on_bad_refreshms_value() {
        let cfg = Config::parse("refreshms = soon\n");
        assert_eq!(cfg.refreshms, 0);
    }

    #[test]
    fn missing_file_yields_defaults() {
        assert_eq!(
            Config::load(Path::new("Z:\\this\\path\\does\\not\\exist.ini")),
            Config::default()
        );
    }
}
