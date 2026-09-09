//! Host-owned environment facts exposed to the native agent.
//!
//! In particular, Linux XDG user directories are configuration, not a
//! translation table. The resolver only reports a configured directory after
//! checking that it currently exists and is a directory.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserDirectory {
    Desktop,
    Documents,
    Downloads,
    Music,
    Pictures,
    Videos,
    PublicShare,
    Templates,
}

impl UserDirectory {
    pub const ALL: [Self; 8] = [
        Self::Desktop,
        Self::Documents,
        Self::Downloads,
        Self::Music,
        Self::Pictures,
        Self::Videos,
        Self::PublicShare,
        Self::Templates,
    ];

    pub const fn xdg_key(self) -> &'static str {
        match self {
            Self::Desktop => "XDG_DESKTOP_DIR",
            Self::Documents => "XDG_DOCUMENTS_DIR",
            Self::Downloads => "XDG_DOWNLOAD_DIR",
            Self::Music => "XDG_MUSIC_DIR",
            Self::Pictures => "XDG_PICTURES_DIR",
            Self::Videos => "XDG_VIDEOS_DIR",
            Self::PublicShare => "XDG_PUBLICSHARE_DIR",
            Self::Templates => "XDG_TEMPLATES_DIR",
        }
    }

    pub const fn json_key(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Documents => "documents",
            Self::Downloads => "downloads",
            Self::Music => "music",
            Self::Pictures => "pictures",
            Self::Videos => "videos",
            Self::PublicShare => "public_share",
            Self::Templates => "templates",
        }
    }

    pub fn from_json_key(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "desktop" => Some(Self::Desktop),
            "documents" => Some(Self::Documents),
            "downloads" => Some(Self::Downloads),
            "pictures" => Some(Self::Pictures),
            "music" => Some(Self::Music),
            "videos" => Some(Self::Videos),
            "public_share" | "public" => Some(Self::PublicShare),
            "templates" => Some(Self::Templates),
            _ => None,
        }
    }

    pub const fn prompt_label(self) -> &'static str {
        match self {
            Self::Desktop => "Desktop",
            Self::Documents => "Documents",
            Self::Downloads => "Downloads",
            Self::Music => "Music",
            Self::Pictures => "Pictures",
            Self::Videos => "Videos",
            Self::PublicShare => "Public share",
            Self::Templates => "Templates",
        }
    }

    pub const fn conventional_name(self) -> &'static str {
        match self {
            Self::Desktop => "Desktop",
            Self::Documents => "Documents",
            Self::Downloads => "Downloads",
            Self::Music => "Music",
            Self::Pictures => "Pictures",
            Self::Videos => "Videos",
            Self::PublicShare => "Public",
            Self::Templates => "Templates",
        }
    }
}

/// Existing, validated user directories. `None` means the host did not find
/// a configured directory that currently exists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserDirectories {
    pub desktop: Option<PathBuf>,
    pub documents: Option<PathBuf>,
    pub downloads: Option<PathBuf>,
    pub music: Option<PathBuf>,
    pub pictures: Option<PathBuf>,
    pub videos: Option<PathBuf>,
    pub public_share: Option<PathBuf>,
    pub templates: Option<PathBuf>,
}

impl UserDirectories {
    pub fn get(&self, directory: UserDirectory) -> Option<&Path> {
        let path = match directory {
            UserDirectory::Desktop => self.desktop.as_deref(),
            UserDirectory::Documents => self.documents.as_deref(),
            UserDirectory::Downloads => self.downloads.as_deref(),
            UserDirectory::Music => self.music.as_deref(),
            UserDirectory::Pictures => self.pictures.as_deref(),
            UserDirectory::Videos => self.videos.as_deref(),
            UserDirectory::PublicShare => self.public_share.as_deref(),
            UserDirectory::Templates => self.templates.as_deref(),
        };
        path
    }

    fn set(&mut self, directory: UserDirectory, path: Option<PathBuf>) {
        match directory {
            UserDirectory::Desktop => self.desktop = path,
            UserDirectory::Documents => self.documents = path,
            UserDirectory::Downloads => self.downloads = path,
            UserDirectory::Music => self.music = path,
            UserDirectory::Pictures => self.pictures = path,
            UserDirectory::Videos => self.videos = path,
            UserDirectory::PublicShare => self.public_share = path,
            UserDirectory::Templates => self.templates = path,
        }
    }

    pub fn resolved(&self) -> impl Iterator<Item = (UserDirectory, &Path)> {
        UserDirectory::ALL
            .into_iter()
            .filter_map(|directory| self.get(directory).map(|path| (directory, path)))
    }
}

/// The host facts used both by the system prompt and `system.environment`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostEnvironment {
    pub os: &'static str,
    pub architecture: &'static str,
    pub home: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub user_dirs: UserDirectories,
    /// The XDG user-directory file that supplied the resolved paths on Linux.
    /// `None` is expected on non-Linux hosts or when the host has no readable
    /// XDG configuration.
    pub xdg_config_source: Option<PathBuf>,
    pub path_style: &'static str,
    pub path_separator: &'static str,
}

impl HostEnvironment {
    pub fn snapshot() -> Self {
        let home = host_home_dir();
        let user_dirs = home
            .as_deref()
            .map(resolve_user_directories)
            .unwrap_or_default();
        #[cfg(target_os = "linux")]
        let xdg_config_source = home.as_deref().and_then(|home| {
            let xdg_config_home = configured_xdg_home();
            xdg_user_dirs_config_source(home, xdg_config_home.as_deref())
        });
        #[cfg(not(target_os = "linux"))]
        let xdg_config_source = None;
        Self {
            os: std::env::consts::OS,
            architecture: std::env::consts::ARCH,
            home,
            cwd: std::env::current_dir().ok(),
            user_dirs,
            xdg_config_source,
            path_style: host_path_style(),
            path_separator: host_path_separator(),
        }
    }

    /// Machine-readable facts for the read-only native environment tool.
    /// Directory fields are omitted when the host could not resolve an
    /// existing directory; a guessed path must never reach the model.
    pub fn json_value(&self) -> Value {
        let mut value = Map::new();
        value.insert("os".to_string(), Value::String(self.os.to_string()));
        value.insert(
            "architecture".to_string(),
            Value::String(self.architecture.to_string()),
        );
        if let Some(home) = &self.home {
            value.insert(
                "home".to_string(),
                Value::String(home.to_string_lossy().into_owned()),
            );
        }
        if let Some(cwd) = &self.cwd {
            let cwd = Value::String(cwd.to_string_lossy().into_owned());
            value.insert("cwd".to_string(), cwd.clone());
            value.insert("current_working_directory".to_string(), cwd);
        }
        if let Some(source) = &self.xdg_config_source {
            value.insert(
                "xdg_config_source".to_string(),
                Value::String(source.to_string_lossy().into_owned()),
            );
        }
        value.insert(
            "path_separator".to_string(),
            Value::String(self.path_separator.to_string()),
        );
        value.insert(
            "path_style".to_string(),
            Value::String(self.path_style.to_ascii_lowercase()),
        );
        for (directory, path) in self.user_dirs.resolved() {
            value.insert(
                directory.json_key().to_string(),
                Value::String(path.to_string_lossy().into_owned()),
            );
        }
        Value::Object(value)
    }
}

/// Resolve user directories using this host's environment and filesystem.
pub fn resolve_user_directories(home: &Path) -> UserDirectories {
    #[cfg(target_os = "linux")]
    {
        resolve_linux_user_directories_from_config(home, configured_xdg_home().as_deref())
    }

    #[cfg(not(target_os = "linux"))]
    {
        resolve_conventional_user_directories(home)
    }
}

/// Testable Linux resolver. Passing `Some(contents)` avoids reading the real
/// user's configuration; `None` means no XDG assignments were available.
#[cfg(target_os = "linux")]
pub fn resolve_linux_user_directories(
    home: &Path,
    config_contents: Option<&str>,
) -> UserDirectories {
    let configured = config_contents
        .map(|contents| parse_xdg_user_dirs(home, contents))
        .unwrap_or_default();
    let mut resolved = UserDirectories::default();

    for directory in UserDirectory::ALL {
        // Linux user directories are authoritative only when supplied by the
        // XDG configuration. An existing ~/Desktop is not evidence: it may be
        // a stale directory created by an older application version.
        let path = configured.get(directory).map(Path::to_path_buf);
        resolved.set(directory, path);
    }
    resolved
}

/// Testable Linux resolver that exercises the same config-file lookup as the
/// host, with an injected `XDG_CONFIG_HOME` directory. Passing `None` uses
/// only `$HOME/.config/user-dirs.dirs`; no process environment is consulted.
#[cfg(target_os = "linux")]
pub fn resolve_linux_user_directories_from_config(
    home: &Path,
    xdg_config_home: Option<&Path>,
) -> UserDirectories {
    let contents = xdg_user_dirs_config_source(home, xdg_config_home)
        .and_then(|source| std::fs::read_to_string(source).ok());
    resolve_linux_user_directories(home, contents.as_deref())
}

#[cfg(target_os = "linux")]
fn xdg_user_dirs_config_source(home: &Path, xdg_config_home: Option<&Path>) -> Option<PathBuf> {
    let mut candidates = Vec::with_capacity(2);
    if let Some(config_home) = xdg_config_home {
        candidates.push(config_home.join("user-dirs.dirs"));
    }
    candidates.push(home.join(".config/user-dirs.dirs"));
    candidates
        .into_iter()
        .find(|path| std::fs::read_to_string(path).is_ok())
}

/// Parse the XDG user-dirs file and retain only existing directories.
///
/// The XDG format uses shell-like assignments. This parser intentionally
/// accepts the forms emitted by `xdg-user-dirs-update`, including quoted
/// values with spaces and both `$HOME` and `${HOME}` expansion, without
/// executing shell code.
#[cfg(target_os = "linux")]
pub fn parse_xdg_user_dirs(home: &Path, contents: &str) -> UserDirectories {
    let mut resolved = UserDirectories::default();
    for line in contents.lines() {
        let Some((key, raw_value)) = line.split_once('=') else {
            continue;
        };
        let Some(directory) = UserDirectory::ALL
            .into_iter()
            .find(|directory| directory.xdg_key() == key.trim())
        else {
            continue;
        };
        let Some(value) = parse_assignment_value(raw_value.trim()) else {
            continue;
        };
        let Some(path) = expand_home(&value, home) else {
            continue;
        };
        if let Some(path) = existing_directory(&path) {
            resolved.set(directory, Some(path));
        }
    }
    resolved
}

#[cfg(target_os = "linux")]
fn configured_xdg_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(target_os = "linux")]
fn parse_assignment_value(raw: &str) -> Option<String> {
    let first = raw.chars().next()?;
    if first == '"' || first == '\'' {
        let quote = first;
        let mut value = String::new();
        let mut escaped = false;
        let mut closing_end = None;
        for (index, character) in raw.char_indices().skip(1) {
            if escaped {
                value.push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == quote {
                closing_end = Some(index + character.len_utf8());
                break;
            } else {
                value.push(character);
            }
        }
        let closing_end = closing_end?;
        if escaped {
            return None;
        }
        // Anything after a quoted assignment must be whitespace or a
        // comment. This avoids accepting malformed shell-like lines.
        let remainder = raw.get(closing_end..)?.trim();
        if !remainder.is_empty() && !remainder.starts_with('#') {
            return None;
        }
        return Some(value);
    }

    Some(raw.split('#').next()?.trim().to_string()).filter(|value| !value.is_empty())
}

#[cfg(target_os = "linux")]
fn expand_home(value: &str, home: &Path) -> Option<PathBuf> {
    let home = home.to_string_lossy();
    let expanded = value
        .replace("${HOME}", home.as_ref())
        .replace("$HOME", home.as_ref());
    let path = PathBuf::from(expanded);
    path.is_absolute().then_some(path)
}

fn existing_directory(path: &Path) -> Option<PathBuf> {
    path.is_dir().then(|| path.to_path_buf())
}

#[cfg(not(target_os = "linux"))]
fn resolve_conventional_user_directories(home: &Path) -> UserDirectories {
    let mut resolved = UserDirectories::default();
    for directory in UserDirectory::ALL {
        resolved.set(
            directory,
            existing_directory(&home.join(directory.conventional_name())),
        );
    }
    resolved
}

#[cfg(target_os = "windows")]
fn host_home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .or_else(|| {
            let drive = std::env::var_os("HOMEDRIVE")?;
            let path = std::env::var_os("HOMEPATH")?;
            Some(PathBuf::from(drive).join(path))
        })
}

#[cfg(not(target_os = "windows"))]
fn host_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub fn host_os_label() -> &'static str {
    match std::env::consts::OS {
        "linux" => "Linux",
        "windows" => "Windows",
        "macos" => "macOS",
        other => other,
    }
}

pub fn host_path_style() -> &'static str {
    if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "POSIX"
    }
}

pub fn host_path_separator() -> &'static str {
    if cfg!(target_os = "windows") {
        "\\"
    } else {
        "/"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    fn temp_home(name: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("utsuwa-xdg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_quoted_home_and_braced_home_assignments() {
        let home = temp_home("quoted");
        let desktop = home.join("Escritorio");
        let documents = home.join("Documentos");
        std::fs::create_dir_all(&desktop).unwrap();
        std::fs::create_dir_all(&documents).unwrap();
        let dirs = parse_xdg_user_dirs(
            &home,
            "XDG_DESKTOP_DIR=\"$HOME/Escritorio\"\nXDG_DOCUMENTS_DIR=\"${HOME}/Documentos\"\n",
        );
        assert_eq!(dirs.desktop.as_deref(), Some(desktop.as_path()));
        assert_eq!(dirs.documents.as_deref(), Some(documents.as_path()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn preserves_spaces_unicode_and_whitespace_in_assignments() {
        let home = temp_home("unicode");
        let pictures = home.join("Imágenes de casa");
        let music = home.join("Música");
        let videos = home.join("Vídeos");
        let downloads = home.join("Descargas");
        let public_share = home.join("Público");
        let templates = home.join("Plantillas");
        for path in [
            &pictures,
            &music,
            &videos,
            &downloads,
            &public_share,
            &templates,
        ] {
            std::fs::create_dir_all(path).unwrap();
        }
        let dirs = parse_xdg_user_dirs(
            &home,
            "  XDG_PICTURES_DIR = \"$HOME/Imágenes de casa\" # comment\n\
             XDG_MUSIC_DIR=\"${HOME}/Música\"\n\
             XDG_VIDEOS_DIR = '$HOME/Vídeos'\n\
             XDG_DOWNLOAD_DIR=\"${HOME}/Descargas\"\n\
             XDG_PUBLICSHARE_DIR=\"$HOME/Público\"\n\
             XDG_TEMPLATES_DIR=\"$HOME/Plantillas\"\n",
        );
        assert_eq!(dirs.pictures.as_deref(), Some(pictures.as_path()));
        assert_eq!(dirs.music.as_deref(), Some(music.as_path()));
        assert_eq!(dirs.videos.as_deref(), Some(videos.as_path()));
        assert_eq!(dirs.downloads.as_deref(), Some(downloads.as_path()));
        assert_eq!(dirs.public_share.as_deref(), Some(public_share.as_path()));
        assert_eq!(dirs.templates.as_deref(), Some(templates.as_path()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_configured_directory_is_not_reported_or_created() {
        let home = temp_home("missing");
        let dirs =
            resolve_linux_user_directories(&home, Some("XDG_DESKTOP_DIR=\"$HOME/Escritorio\"\n"));
        assert!(dirs.desktop.is_none());
        assert!(!home.join("Escritorio").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_does_not_infer_conventional_desktop_without_xdg_configuration() {
        let home = temp_home("no-fallback");
        std::fs::create_dir_all(home.join("Desktop")).unwrap();
        assert!(resolve_linux_user_directories(&home, None)
            .desktop
            .is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn config_file_lookup_accepts_an_injected_xdg_config_home() {
        let home = temp_home("config-path");
        let config_home = home.join("custom-config");
        let desktop = home.join("Escritorio");
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::create_dir_all(&desktop).unwrap();
        std::fs::write(
            config_home.join("user-dirs.dirs"),
            "XDG_DESKTOP_DIR=\"$HOME/Escritorio\"\n",
        )
        .unwrap();

        let dirs = resolve_linux_user_directories_from_config(&home, Some(&config_home));
        assert_eq!(dirs.desktop.as_deref(), Some(desktop.as_path()));
        assert!(dirs.documents.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn config_file_lookup_falls_back_to_home_config() {
        let home = temp_home("home-config");
        let documents = home.join("Documentos");
        std::fs::create_dir_all(home.join(".config")).unwrap();
        std::fs::create_dir_all(&documents).unwrap();
        std::fs::write(
            home.join(".config/user-dirs.dirs"),
            "XDG_DOCUMENTS_DIR=\"${HOME}/Documentos\"\n",
        )
        .unwrap();

        let dirs = resolve_linux_user_directories_from_config(&home, Some(&home.join("missing")));
        assert_eq!(dirs.documents.as_deref(), Some(documents.as_path()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn config_source_prefers_xdg_config_home_and_falls_back_to_home_config() {
        let home = temp_home("source");
        let xdg_config_home = home.join("xdg-config");
        std::fs::create_dir_all(&xdg_config_home).unwrap();
        std::fs::create_dir_all(home.join(".config")).unwrap();
        std::fs::write(
            xdg_config_home.join("user-dirs.dirs"),
            "XDG_DESKTOP_DIR=\"$HOME/xdg-desktop\"\n",
        )
        .unwrap();
        std::fs::write(
            home.join(".config/user-dirs.dirs"),
            "XDG_DESKTOP_DIR=\"$HOME/home-desktop\"\n",
        )
        .unwrap();

        assert_eq!(
            xdg_user_dirs_config_source(&home, Some(&xdg_config_home)),
            Some(xdg_config_home.join("user-dirs.dirs"))
        );
        assert_eq!(
            xdg_user_dirs_config_source(&home, Some(&home.join("missing"))),
            Some(home.join(".config/user-dirs.dirs"))
        );
    }

    #[test]
    fn environment_json_uses_machine_friendly_path_style() {
        let value = HostEnvironment::snapshot().json_value();
        assert_eq!(value["os"], std::env::consts::OS);
        assert_eq!(value["path_style"], host_path_style().to_ascii_lowercase());
    }
}
