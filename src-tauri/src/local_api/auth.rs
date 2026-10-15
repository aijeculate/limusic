//! The checks every request passes before routing. Pure, so they're unit-tested without a socket.

use hyper::header::{AUTHORIZATION, HOST, ORIGIN};
use hyper::HeaderMap;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Denied {
    /// Any `Origin` at all: only a browser sends one, and no browser page is a client.
    Origin,
    /// `Host` isn't loopback: a DNS-rebinding page names its own host here.
    Host,
    Token,
}

pub(super) fn check(headers: &HeaderMap, port: u16, token: &str) -> Result<(), Denied> {
    if headers.contains_key(ORIGIN) {
        return Err(Denied::Origin);
    }
    let host = headers.get(HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    if !host_ok(host, port) {
        return Err(Denied::Host);
    }
    let given = headers
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    if token.is_empty() || !constant_time_eq(given.as_bytes(), token.as_bytes()) {
        return Err(Denied::Token);
    }
    Ok(())
}

fn host_ok(host: &str, port: u16) -> bool {
    let host = host.to_ascii_lowercase();
    host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
}

/// No early exit on the first differing byte, so response timing says nothing about the token.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::HeaderValue;

    const PORT: u16 = 26538;
    const TOKEN: &str = "abc123";

    fn headers(pairs: &[(hyper::header::HeaderName, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(k.clone(), HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn accepts_a_good_request() {
        let h = headers(&[(HOST, "127.0.0.1:26538"), (AUTHORIZATION, "Bearer abc123")]);
        assert_eq!(check(&h, PORT, TOKEN), Ok(()));
        let h = headers(&[(HOST, "LocalHost:26538"), (AUTHORIZATION, "Bearer abc123")]);
        assert_eq!(check(&h, PORT, TOKEN), Ok(()));
    }

    #[test]
    fn rejects_missing_or_wrong_token() {
        let h = headers(&[(HOST, "127.0.0.1:26538")]);
        assert_eq!(check(&h, PORT, TOKEN), Err(Denied::Token));
        let h = headers(&[(HOST, "127.0.0.1:26538"), (AUTHORIZATION, "Bearer abc124")]);
        assert_eq!(check(&h, PORT, TOKEN), Err(Denied::Token));
        let h = headers(&[(HOST, "127.0.0.1:26538"), (AUTHORIZATION, "abc123")]);
        assert_eq!(check(&h, PORT, TOKEN), Err(Denied::Token));
    }

    #[test]
    fn an_empty_configured_token_matches_nothing() {
        let h = headers(&[(HOST, "127.0.0.1:26538"), (AUTHORIZATION, "Bearer ")]);
        assert_eq!(check(&h, PORT, ""), Err(Denied::Token));
    }

    #[test]
    fn rejects_browser_origins_even_with_the_token() {
        let h = headers(&[
            (HOST, "127.0.0.1:26538"),
            (AUTHORIZATION, "Bearer abc123"),
            (ORIGIN, "https://evil.example"),
        ]);
        assert_eq!(check(&h, PORT, TOKEN), Err(Denied::Origin));
    }

    #[test]
    fn rejects_rebinding_hosts() {
        for host in ["evil.example:26538", "127.0.0.1:80", "127.0.0.1", ""] {
            let h = headers(&[(HOST, host), (AUTHORIZATION, "Bearer abc123")]);
            assert_eq!(check(&h, PORT, TOKEN), Err(Denied::Host), "{host}");
        }
    }
}
