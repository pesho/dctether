//! URL opening: validation on the host, the `BROWSER` helper in the container.

/// Longest URL the host will open.
pub const MAX_URL: usize = 8192;

/// Check a URL sent by the (untrusted) container before it is handed to
/// `xdg-open`/`open` as a single argument. Only `http://` and `https://` are
/// accepted, and only printable ASCII without spaces, so nothing can be
/// read as an option, a file path or another scheme.
pub fn validate_url(url: &str) -> Result<(), &'static str> {
    if url.len() > MAX_URL {
        return Err("URL too long");
    }
    if !url.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err("URL contains spaces, control or non-ASCII characters");
    }
    let lower = url.to_ascii_lowercase();
    let rest = if let Some(r) = lower.strip_prefix("https://") {
        r
    } else if let Some(r) = lower.strip_prefix("http://") {
        r
    } else {
        return Err("only http:// and https:// URLs are opened");
    };
    if rest.is_empty() || rest.starts_with('/') {
        return Err("URL has no host");
    }
    Ok(())
}

/// The command that opens a URL on this host.
pub fn opener() -> &'static str {
    if cfg!(target_os = "macos") { "open" } else { "xdg-open" }
}

/// Container side: `dctether-browser URL…` (what `BROWSER` points to).
/// Sends each URL to the session's browser socket and reports the host's
/// answer.
#[cfg(target_os = "linux")]
pub fn helper_main(args: &[String]) -> i32 {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    if args.is_empty() {
        eprintln!("usage: dctether-browser URL...");
        return 2;
    }
    let Some(session) = std::env::var_os("DCTETHER_SESSION") else {
        eprintln!("dctether-browser: DCTETHER_SESSION is not set (not inside a dctether session?)");
        return 1;
    };
    let sock = std::path::Path::new(&session).join("browser.sock");
    let mut status = 0;
    for url in args {
        let result = (|| -> std::io::Result<String> {
            let mut s = UnixStream::connect(&sock)?;
            s.write_all(url.as_bytes())?;
            s.shutdown(std::net::Shutdown::Write)?;
            let mut reply = String::new();
            s.take(4096).read_to_string(&mut reply)?;
            Ok(reply)
        })();
        match result {
            Ok(r) if r.trim_end() == "ok" => {}
            Ok(r) => {
                eprintln!("dctether-browser: {}: {}", url, r.trim_end());
                status = 1;
            }
            Err(e) => {
                eprintln!("dctether-browser: {}: {}", sock.display(), e);
                status = 1;
            }
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_http_and_https() {
        for u in [
            "http://example.com",
            "https://example.com/path?q=1&r=%20#frag",
            "HTTPS://EXAMPLE.COM/",
            "http://127.0.0.1:8080/callback?code=abc",
            "https://[::1]:3000/",
        ] {
            assert_eq!(validate_url(u), Ok(()), "{u}");
        }
    }

    #[test]
    fn rejects_everything_else() {
        for u in [
            "",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ftp://example.com",
            "-e http://x",
            "--help",
            "/usr/bin/xterm",
            "http:/example.com",
            "http://",
            "http:///etc/passwd",
            "https://exa mple.com",
            "https://example.com/\n",
            "https://example.com/\u{0}",
            "https://bücher.example/",
            " https://example.com",
        ] {
            assert!(validate_url(u).is_err(), "{u:?}");
        }
        assert!(validate_url(&format!("https://e.com/{}", "a".repeat(MAX_URL))).is_err());
    }
}
