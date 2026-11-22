//! Authentication and browser-origin checks for the optional LAN listener.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use subtle::ConstantTimeEq;
use url::{Host, Url};

/// Authentication is mandatory even for static assets and signaling.
pub fn authenticate(header: Option<&str>, username: &str, password: &str) -> bool {
    let Some(encoded) = header.and_then(|h| h.strip_prefix("Basic ")) else {
        return false;
    };
    let Ok(decoded) = STANDARD.decode(encoded) else {
        return false;
    };
    let Some(split) = decoded.iter().position(|b| *b == b':') else {
        return false;
    };
    let (user, pass) = decoded.split_at(split);
    bool::from(user.ct_eq(username.as_bytes()) & pass[1..].ct_eq(password.as_bytes()))
}

/// Accept IP literals and localhost, plus explicitly configured proxy hostnames.
/// Names supplied by a request are never resolved: that would enable DNS rebinding.
pub fn allowed_host(authority: &str, port: u16, extra_hosts: &[String]) -> bool {
    if authority.contains(['/', '@', '?', '#', '\\']) {
        return false;
    }
    let Ok(url) = Url::parse(&format!("http://{authority}/")) else {
        return false;
    };
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    if extra_hosts
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(authority))
    {
        return true;
    }
    if url.port_or_known_default() != Some(port) {
        return false;
    }
    match url.host() {
        Some(Host::Ipv4(_)) | Some(Host::Ipv6(_)) => true,
        Some(Host::Domain(host)) => host == "localhost",
        None => false,
    }
}

/// Mutations require an Origin matching the validated request authority exactly.
/// A trusted reverse proxy must preserve Host and Origin (and explicitly allow its host).
pub fn same_origin(origin: Option<&str>, authority: &str) -> bool {
    let Some(origin) = origin else { return false };
    let Ok(url) = Url::parse(origin) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https")
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    let Ok(target) = Url::parse(&format!("{}://{authority}/", url.scheme())) else {
        return false;
    };
    url.origin() == target.origin()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_auth_requires_exact_credentials() {
        assert!(authenticate(Some("Basic dXNlcjpwYXNz"), "user", "pass"));
        assert!(!authenticate(Some("Basic dXNlcjpwYXNz"), "user", "wrong"));
        assert!(!authenticate(None, "user", "pass"));
    }

    #[test]
    fn rejects_rebinding_and_cross_origin() {
        assert!(allowed_host("192.168.1.2:8080", 8080, &[]));
        assert!(!allowed_host("attacker.example:8080", 8080, &[]));
        assert!(!allowed_host("user@192.168.1.2:8080", 8080, &[]));
        assert!(same_origin(
            Some("http://192.168.1.2:8080"),
            "192.168.1.2:8080"
        ));
        assert!(same_origin(Some("http://[::1]:8080"), "[::1]:8080"));
        assert!(same_origin(Some("https://example.org"), "example.org:443"));
        assert!(!same_origin(
            Some("http://attacker.example:8080"),
            "192.168.1.2:8080"
        ));
        assert!(!same_origin(None, "192.168.1.2:8080"));
    }
}
