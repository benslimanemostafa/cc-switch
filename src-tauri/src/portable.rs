use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::OnceLock;

#[derive(Clone, Debug)]
struct PortablePaths {
    data: PathBuf,
    home: PathBuf,
    config: PathBuf,
}

static PORTABLE_PATHS: OnceLock<Option<PortablePaths>> = OnceLock::new();

/// Initialize the strict D-drive portable layout before Tauri or any plugin starts.
/// A present marker that is malformed, on another drive, or redirected through a
/// reparse point is a fatal error; it never falls back to the Windows user profile.
pub fn initialize() -> Result<(), String> {
    if PORTABLE_PATHS.get().is_some() {
        return Ok(());
    }

    let executable = env::current_exe()
        .map_err(|error| format!("cannot resolve executable path: {error}"))?;
    let executable = fs::canonicalize(&executable)
        .map_err(|error| format!("cannot canonicalize executable path: {error}"))?;
    let executable_dir = executable
        .parent()
        .ok_or_else(|| "executable has no parent directory".to_string())?;

    let marker = executable_dir.join("portable.ini");
    match fs::metadata(&marker) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            PORTABLE_PATHS
                .set(None)
                .map_err(|_| "portable mode was already initialized".to_string())?;
            return Ok(());
        }
        Err(error) => {
            return Err(format!("cannot inspect portable marker {}: {error}", marker.display()));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(format!("portable marker is not a file: {}", marker.display()));
        }
        Ok(_) => {}
    }

    let marker_text = fs::read_to_string(&marker)
        .map_err(|error| format!("cannot read portable marker {}: {error}", marker.display()))?;
    let settings = parse_marker(&marker_text)?;
    if settings.0 != "true" || settings.1.to_ascii_uppercase() != "D:" {
        return Err("portable marker must contain portable=true and required_drive=D:".into());
    }

    if !executable_dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("App"))
    {
        return Err("portable executable must be inside the App directory".into());
    }

    let app_root = executable_dir
        .parent()
        .ok_or_else(|| "portable App directory has no parent".to_string())?;
    let app_root = fs::canonicalize(app_root)
        .map_err(|error| format!("cannot canonicalize portable root: {error}"))?;

    if drive_letter(&app_root) != Some('D') {
        return Err(format!(
            "portable build is restricted to D:, resolved root is {}",
            app_root.display()
        ));
    }

    // Reject junctions/symlinks in the data tree rather than following them to C:.
    let data = ensure_directory(&app_root, "data")?;
    let config = ensure_directory(&data, "config")?;
    let _appdata = ensure_directory(&data, "appdata")?;
    let _webview2 = ensure_directory(&data, "webview2")?;
    let _cache = ensure_directory(&data, "cache")?;
    let _logs = ensure_directory(&data, "logs")?;
    let temp = ensure_directory(&data, "temp")?;
    let home = ensure_directory(&data, "home")?;
    let profile = ensure_directory(&data, "profile")?;
    let roaming = ensure_directory(&profile, "roaming")?;
    let local = ensure_directory(&profile, "local")?;
    let xdg_config = ensure_directory(&profile, "config")?;
    let xdg_cache = ensure_directory(&profile, "cache")?;
    let xdg_data = ensure_directory(&profile, "data")?;

    let home_path = home.to_string_lossy().into_owned();
    let temp_path = temp.to_string_lossy().into_owned();
    let roaming_path = roaming.to_string_lossy().into_owned();
    let local_path = local.to_string_lossy().into_owned();

    // These are process-only values inherited by child processes. Nothing is
    // written to the user's global Windows environment or registry.
    env::set_var("USERPROFILE", &home_path);
    env::set_var("HOME", &home_path);
    env::set_var("HOMEDRIVE", "D:");
    env::set_var(
        "HOMEPATH",
        home_path.strip_prefix("D:").unwrap_or(&home_path),
    );
    env::set_var("APPDATA", &roaming_path);
    env::set_var("LOCALAPPDATA", &local_path);
    env::set_var("TEMP", &temp_path);
    env::set_var("TMP", &temp_path);
    env::set_var("TMPDIR", &temp_path);
    env::set_var("XDG_CONFIG_HOME", xdg_config);
    env::set_var("XDG_CACHE_HOME", xdg_cache);
    env::set_var("XDG_DATA_HOME", xdg_data);

    PORTABLE_PATHS
        .set(Some(PortablePaths { data, home, config }))
        .map_err(|_| "portable mode was already initialized".to_string())?;

    Ok(())
}

fn parse_marker(contents: &str) -> Result<(String, String), String> {
    let mut portable = None;
    let mut required_drive = None;

    for line in contents.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "portable" => portable = Some(value.trim().to_string()),
            "required_drive" => required_drive = Some(value.trim().to_string()),
            _ => {}
        }
    }

    Ok((
        portable.ok_or_else(|| "portable marker is missing portable=true".to_string())?,
        required_drive
            .ok_or_else(|| "portable marker is missing required_drive=D:".to_string())?,
    ))
}

fn drive_letter(path: &Path) -> Option<char> {
    match path.components().next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                Some((letter as char).to_ascii_uppercase())
            }
            _ => None,
        },
        _ => None,
    }
}

fn ensure_directory(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| format!("cannot canonicalize {}: {error}", root.display()))?;
    let mut current = canonical_root.clone();

    for component in Path::new(relative).components() {
        let Component::Normal(name) = component else {
            return Err(format!("invalid portable directory component: {relative}"));
        };
        current.push(name);

        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if is_reparse_point(&metadata) {
                    return Err(format!(
                        "portable data path contains a junction or symbolic link: {}",
                        current.display()
                    ));
                }
                if !metadata.is_dir() {
                    return Err(format!(
                        "portable data path is not a directory: {}",
                        current.display()
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)
                    .map_err(|create_error| format!("cannot create {}: {create_error}", current.display()))?;
            }
            Err(error) => {
                return Err(format!("cannot inspect {}: {error}", current.display()));
            }
        }

        let resolved = fs::canonicalize(&current)
            .map_err(|error| format!("cannot canonicalize {}: {error}", current.display()))?;
        if !resolved.starts_with(&canonical_root) {
            return Err(format!(
                "portable data directory escapes its D: root: {}",
                resolved.display()
            ));
        }
        current = resolved;
    }

    Ok(current)
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

pub fn is_portable_mode() -> bool {
    matches!(PORTABLE_PATHS.get(), Some(Some(_)))
}

pub fn home_dir() -> Option<PathBuf> {
    PORTABLE_PATHS
        .get()
        .and_then(Option::as_ref)
        .map(|paths| paths.home.clone())
}

pub fn app_config_dir() -> Option<PathBuf> {
    PORTABLE_PATHS
        .get()
        .and_then(Option::as_ref)
        .map(|paths| paths.config.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_requires_portable_and_drive() {
        assert_eq!(
            parse_marker("# comment\nportable=true\nrequired_drive=D:\n").unwrap(),
            ("true".into(), "D:".into())
        );
        assert!(parse_marker("portable=true\n").is_err());
    }

    #[test]
    fn drive_letter_recognizes_windows_disk_prefix() {
        assert_eq!(drive_letter(Path::new(r"D:\Tools\CCSwitch")), Some('D'));
        assert_eq!(drive_letter(Path::new(r"C:\Users\Example")), Some('C'));
    }
}
