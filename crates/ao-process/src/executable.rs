//! Resolve Windows npm shims before passing them to Command. Rust handles the
//! .cmd/.bat interpreter and argument quoting; do not build a cmd /C string.
use std::path::PathBuf;

pub fn resolve(command: &str, search_path: &std::ffi::OsStr) -> PathBuf {
    #[cfg(windows)]
    {
        let path = std::path::Path::new(command);
        if path.extension().is_none() {
            let candidates: Vec<PathBuf> = if path.components().count() > 1 {
                vec![path.to_path_buf()]
            } else {
                std::env::split_paths(search_path)
                    .map(|dir| dir.join(command))
                    .collect()
            };
            for candidate in candidates {
                for extension in ["exe", "cmd", "bat"] {
                    let candidate = candidate.with_extension(extension);
                    if candidate.is_file() {
                        return candidate;
                    }
                }
            }
        }
    }
    #[cfg(not(windows))]
    let _ = search_path;
    command.into()
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn finds_npm_shim_and_prefers_native_executable() {
        let dir = std::env::temp_dir().join(format!("launchpad-shim-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("codex.cmd"), "@echo off").unwrap();
        assert_eq!(resolve("codex", dir.as_os_str()), dir.join("codex.cmd"));
        std::fs::write(dir.join("codex.exe"), "").unwrap();
        assert_eq!(resolve("codex", dir.as_os_str()), dir.join("codex.exe"));
        assert_eq!(
            resolve("native.exe", dir.as_os_str()),
            PathBuf::from("native.exe")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
