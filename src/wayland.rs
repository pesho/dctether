//! Wayland: the host's compositor socket, bind-mounted into the container
//! by `dctether up`.
//!
//! Wayland clients pass file descriptors (buffers, keymaps) over the
//! socket, which a byte relay can't carry. A bind mount of the socket file
//! works because the container shares the host's kernel: clients talk to
//! the compositor directly. A mount can only be added when the container
//! is created, hence `dctether up`.

use std::path::{Path, PathBuf};

/// Where `dctether up` mounts the host socket inside the container.
/// `WAYLAND_DISPLAY` may be an absolute path, so no `XDG_RUNTIME_DIR` is
/// needed in the container.
pub const CONTAINER_SOCKET: &str = "/tmp/dctether-wayland.sock";

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
    fn builds_single_token_mount() {
        assert_eq!(
            mount_arg(Path::new("/run/user/1000/wayland-1")).unwrap(),
            "--mount=type=bind,source=/run/user/1000/wayland-1,target=/tmp/dctether-wayland.sock"
        );
        assert!(mount_arg(Path::new("/run/a,b/wayland-0")).is_err());
        assert!(mount_arg(Path::new("/run/a\nb")).is_err());
    }
}
