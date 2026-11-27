//! Wayland: the host's compositor socket, bind-mounted into the container
//! by `dctether up`.
//!
//! Wayland clients pass file descriptors (buffers, keymaps) over the
//! socket, which a byte relay can't carry. A bind mount of the socket file
//! works because the container shares the host's kernel: clients talk to
//! the compositor directly. A mount can only be added when the container
//! is created, hence `dctether up`. Containers created by VS Code carry its
//! own mount of the same socket, which `dctether exec` uses as a fallback.

use std::path::{Path, PathBuf};

/// Where `dctether up` mounts the host socket inside the container.
/// `WAYLAND_DISPLAY` may be an absolute path, so no `XDG_RUNTIME_DIR` is
/// needed in the container.
pub const CONTAINER_SOCKET: &str = "/tmp/dctether-wayland.sock";

/// VS Code's Dev Containers extension mounts the host socket as
/// `/tmp/vscode-wayland-<uuid>.sock`. That is its internal naming, not a
/// documented interface; if it changes, the fallback simply finds nothing.
const VSCODE_PREFIX: &str = "vscode-wayland-";
const VSCODE_SUFFIX: &str = ".sock";

/// Sockets to try in `dir` (the container's `/tmp`), in order: dctether's
/// own mount, then VS Code's, sorted for a stable choice.
pub fn candidates(dir: &Path) -> Vec<PathBuf> {
    let own = Path::new(CONTAINER_SOCKET).file_name().expect("constant has a file name");
    let mut vscode: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(VSCODE_PREFIX) && n.ends_with(VSCODE_SUFFIX))
        })
        .collect();
    vscode.sort();
    std::iter::once(dir.join(own)).chain(vscode).collect()
}

/// The host compositor socket named by `WAYLAND_DISPLAY`: an absolute path,
/// or a name relative to `XDG_RUNTIME_DIR` (as libwayland resolves it).
pub fn host_socket(wayland_display: Option<&str>, xdg_runtime_dir: Option<&str>) -> Option<PathBuf> {
    let display = wayland_display.filter(|d| !d.is_empty())?;
    if display.starts_with('/') {
        return Some(PathBuf::from(display));
    }
    let dir = xdg_runtime_dir.filter(|d| d.starts_with('/'))?;
    Some(Path::new(dir).join(display))
}

/// The `devcontainer up` argument that mounts `source` at
/// [`CONTAINER_SOCKET`], as one token. Paths with a comma or control
/// characters can't be expressed in the mount syntax and are refused.
pub fn mount_arg(source: &Path) -> Result<String, String> {
    let s = source
        .to_str()
        .ok_or_else(|| format!("{}: not valid UTF-8", source.display()))?;
    if s.contains(',') || s.chars().any(char::is_control) {
        return Err(format!("{s}: can't be used in a mount (comma or control character)"));
    }
    Ok(format!("--mount=type=bind,source={s},target={CONTAINER_SOCKET}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_host_socket_like_libwayland() {
        assert_eq!(
            host_socket(Some("wayland-1"), Some("/run/user/1000")),
            Some(PathBuf::from("/run/user/1000/wayland-1"))
        );
        assert_eq!(
            host_socket(Some("/tmp/custom.sock"), None),
            Some(PathBuf::from("/tmp/custom.sock"))
        );
        assert_eq!(host_socket(Some("wayland-0"), None), None);
        assert_eq!(host_socket(Some("wayland-0"), Some("relative")), None);
        assert_eq!(host_socket(Some(""), Some("/run/user/1000")), None);
        assert_eq!(host_socket(None, Some("/run/user/1000")), None);
    }

    #[test]
    fn lists_own_socket_then_vscode_ones() {
        let dir = std::env::temp_dir().join(format!("dctether-wl-cand-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in [
            "vscode-wayland-b.sock",
            "vscode-wayland-a.sock",
            "vscode-wayland-x.lock",
            "vscode-ssh-auth-1.sock",
            "other.sock",
        ] {
            std::fs::write(dir.join(f), b"").unwrap();
        }
        assert_eq!(
            candidates(&dir),
            vec![
                dir.join("dctether-wayland.sock"),
                dir.join("vscode-wayland-a.sock"),
                dir.join("vscode-wayland-b.sock"),
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
        // A missing directory still yields dctether's own path.
        assert_eq!(candidates(&dir), vec![dir.join("dctether-wayland.sock")]);
        assert_eq!(candidates(Path::new("/tmp"))[0], Path::new(CONTAINER_SOCKET));
    }

    #[test]
    fn builds_single_token_mount() {
        assert_eq!(
            mount_arg(Path::new("/run/user/1000/wayland-1")).unwrap(),
            "--mount=type=bind,source=/run/user/1000/wayland-1,target=/tmp/dctether-wayland.sock"
        );
        assert!(mount_arg(Path::new("/run/a,b/wayland-0")).is_err());
        assert!(mount_arg(Path::new("/run/a\nb")).is_err());
    }
}
