//! Platform shell resolution. Windows preview requires Git for Windows.
use std::path::PathBuf;

pub fn bash() -> std::io::Result<PathBuf> {
    #[cfg(windows)]
    {
        // Never select System32/bash.exe: that is the legacy WSL launcher.
        if let Some(path) = std::env::var_os("LAUNCHPAD_GIT_BASH") {
            let path = PathBuf::from(path);
            if path.is_file() {
                return Ok(path);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "LAUNCHPAD_GIT_BASH must point to Git for Windows bash.exe",
            ));
        }
        let mut candidates = Vec::new();
        for key in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(root) = std::env::var_os(key) {
                let root = PathBuf::from(root);
                candidates.push(root.join("Git/bin/bash.exe"));
                candidates.push(root.join("Programs/Git/bin/bash.exe"));
            }
        }
        if let Some(path) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path) {
                candidates.push(dir.join("bash.exe"));
                candidates.push(dir.join("../bin/bash.exe"));
            }
        }
        if let Some(path) = candidates.into_iter().find(|p| {
            p.is_file()
                && !p
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains("system32")
        }) {
            return Ok(path);
        }
        Err(std::io::Error::new(std::io::ErrorKind::NotFound,
            "Git Bash was not found. Install Git for Windows or set LAUNCHPAD_GIT_BASH to its bin\\bash.exe."))
    }
    #[cfg(not(windows))]
    {
        if let Ok(shell) = std::env::var("SHELL") {
            if shell.contains("bash") {
                return Ok(PathBuf::from(shell));
            }
        }
        Ok(PathBuf::from("/bin/bash"))
    }
}

/// Paths embedded in Bash source must use forward slashes on Windows.
pub fn bash_path(path: &std::path::Path) -> String {
    let path = path.to_string_lossy().into_owned();
    #[cfg(windows)]
    let path = path.replace('\\', "/");
    path.replace('\'', "'\\''")
}

/// Convert paths in Bash environment values without changing Unix bytes.
pub fn bash_env_path(path: &std::path::Path) -> std::ffi::OsString {
    #[cfg(windows)]
    {
        std::ffi::OsString::from(path.to_string_lossy().replace('\\', "/"))
    }
    #[cfg(not(windows))]
    {
        path.as_os_str().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_path_quotes_apostrophes() {
        assert_eq!(bash_path(std::path::Path::new("a'b")), "a'\\''b");
    }
}
