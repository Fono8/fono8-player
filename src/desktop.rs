//! Linux desktop integration: on every start Fono8 registers itself in the
//! application menu (`fono8.desktop`) and installs its icons, so the launcher,
//! the dock and Alt+Tab show it without a separate install step. Started from
//! an AppImage, the entry points to the `.AppImage` file, not to its temporary
//! mount. An existing entry is replaced only if Fono8 generated it.

use std::path::{Path, PathBuf};

const MARKER: &str = "X-Fono8-Generated=true";
const SVG: &[u8] = include_bytes!("../assets/fono8.svg");
const PNGS: &[(u32, &[u8])] = &[
    (16, include_bytes!("../assets/app-icon/fono8-16.png")),
    (24, include_bytes!("../assets/app-icon/fono8-24.png")),
    (32, include_bytes!("../assets/app-icon/fono8-32.png")),
    (48, include_bytes!("../assets/app-icon/fono8-48.png")),
    (64, include_bytes!("../assets/app-icon/fono8-64.png")),
    (128, include_bytes!("../assets/app-icon/fono8-128.png")),
    (256, include_bytes!("../assets/app-icon/fono8-256.png")),
    (512, include_bytes!("../assets/app-icon/fono8-512.png")),
];

/// Quote a path for the `Exec` key (desktop entry specification).
fn quote(value: &str) -> String {
    let mut quoted = String::from('"');
    for c in value.replace('%', "%%").chars() {
        if matches!(c, '\\' | '"' | '`' | '$') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

/// Escape a desktop entry string value (`\` is the key file's escape character).
fn escape(value: &str) -> String {
    value.replace('\\', "\\\\")
}

pub fn entry(executable: &Path) -> String {
    let path = executable.to_string_lossy();
    let exec = escape(&quote(&path));
    // TryExec is a plain path, not a command line: GLib skips the whole entry
    // (no menu item, no dock icon) when it is quoted.
    let try_exec = escape(&path);
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Fono8\n\
         Comment=Your music. All together.\n\
         Comment[pl]=Twoja muzyka. Wszystko razem.\n\
         Exec={exec} %F\n\
         TryExec={try_exec}\n\
         Icon=fono8\n\
         Terminal=false\n\
         Categories=AudioVideo;Audio;Player;\n\
         Keywords=music;audio;playlist;spotify;youtube;\n\
         StartupWMClass=fono8\n\
         {MARKER}\n"
    )
}

/// The program the menu entry should start, or `None` for test and helper binaries.
fn executable() -> Option<PathBuf> {
    if let Some(appimage) = std::env::var_os("APPIMAGE").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(appimage);
    }
    let exe = std::env::current_exe().ok()?;
    let path = exe.to_string_lossy();
    // `cargo test` binaries live in target/*/deps.
    (!path.contains("/deps/") && !path.contains("/tmp/.mount_")).then_some(exe)
}

/// Write `bytes` unless the file already has them.
fn write_if_changed(path: &Path, bytes: &[u8]) -> std::io::Result<bool> {
    if std::fs::read(path).is_ok_and(|current| current == bytes) {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, bytes)?;
    Ok(true)
}

/// Install or refresh the menu entry and icons under `data` (`$XDG_DATA_HOME`).
pub fn install(data: &Path, executable: &Path) -> std::io::Result<bool> {
    let mut changed = write_if_changed(&data.join("icons/hicolor/scalable/apps/fono8.svg"), SVG)?;
    for (size, png) in PNGS {
        changed |= write_if_changed(&data.join(format!("icons/hicolor/{size}x{size}/apps/fono8.png")), png)?;
    }
    let desktop = data.join("applications/fono8.desktop");
    let ours = match std::fs::read_to_string(&desktop) {
        Ok(current) => current.lines().any(|line| line.trim() == MARKER),
        Err(_) => true,
    };
    if ours {
        changed |= write_if_changed(&desktop, entry(executable).as_bytes())?;
    }
    Ok(changed)
}

/// Register Fono8 for the current user (Linux; errors are only logged).
pub fn integrate() {
    let Some(executable) = executable() else { return };
    let Some(data) =
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).or_else(|| dirs::home_dir().map(|h| h.join(".local/share")))
    else {
        return;
    };
    match install(&data, &executable) {
        Ok(true) => {
            crate::app::debug(|| format!("desktop entry and icons updated for {}", executable.display()));
            // Optional: refresh the MIME/desktop cache where the tool exists.
            let _ = std::process::Command::new("update-desktop-database")
                .arg(data.join("applications"))
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
        Ok(false) => {}
        Err(error) => crate::app::debug(|| format!("desktop integration failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_entry_and_icons_and_keeps_foreign_entries() {
        let data = std::env::temp_dir().join(format!("fono8-desktop-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let exe = Path::new("/opt/My Apps/Fono8-0.1.0-x86_64.AppImage");
        assert!(install(&data, exe).unwrap());
        let desktop = std::fs::read_to_string(data.join("applications/fono8.desktop")).unwrap();
        assert!(desktop.contains("Exec=\"/opt/My Apps/Fono8-0.1.0-x86_64.AppImage\" %F"));
        assert!(desktop.contains("\nTryExec=/opt/My Apps/Fono8-0.1.0-x86_64.AppImage\n"));
        assert!(desktop.contains("Icon=fono8") && desktop.contains(MARKER));
        assert!(data.join("icons/hicolor/256x256/apps/fono8.png").is_file());
        assert!(data.join("icons/hicolor/scalable/apps/fono8.svg").is_file());
        assert!(!install(&data, exe).unwrap(), "nothing to do the second time");
        // A launcher written by someone else is left alone.
        std::fs::write(data.join("applications/fono8.desktop"), "[Desktop Entry]\nExec=/opt/other/launcher.sh\n").unwrap();
        install(&data, Path::new("/usr/bin/fono8")).unwrap();
        assert!(std::fs::read_to_string(data.join("applications/fono8.desktop")).unwrap().contains("launcher.sh"));
        let _ = std::fs::remove_dir_all(&data);
        assert_eq!(quote("/a b/$x%"), "\"/a b/\\$x%%\"");
        assert_eq!(escape(&quote("/a\\b")), "\"/a\\\\\\\\b\"");
    }
}
