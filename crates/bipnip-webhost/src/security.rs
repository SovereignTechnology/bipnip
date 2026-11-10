//! Request checks and response headers (`docs/decisions/0009-web-client-security.md`).
//!
//! - **Host allow-list** on every request, against DNS rebinding: a page on another site that
//!   points its own name at our address still sends its own name in `Host`.
//! - **Same-origin check** on every request that changes state or opens a websocket: the
//!   browser's `Origin` must be exactly one of ours, so another site cannot drive the proxy with
//!   the user's robot tokens (CSRF, cross-site websocket hijacking).
//! - **Headers** on every response; a strict CSP with a per-response nonce on HTML pages.

use axum::http::{HeaderMap, HeaderValue, Method, header};
use base64::Engine;

/// Host names (with port) this server answers to, and the origins they imply.
#[derive(Debug, Clone)]
pub struct Allowed {
    hosts: Vec<String>,
}

impl Allowed {
    /// `hosts` are `host:port` strings exactly as browsers send them in `Host`.
    pub fn new(hosts: Vec<String>) -> Self {
        Allowed {
            hosts: hosts.into_iter().map(|h| h.to_ascii_lowercase()).collect(),
        }
    }

    /// True when the request's `Host` is one of ours.
    pub fn host_ok(&self, headers: &HeaderMap) -> bool {
        headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .map(|h| self.hosts.iter().any(|a| a.eq_ignore_ascii_case(h)))
            .unwrap_or(false)
    }

    /// True when the request may proceed as far as origins go: safe methods without a websocket
    /// upgrade always may; anything else needs `Origin: http://<one of our hosts>`.
    pub fn origin_ok(&self, method: &Method, headers: &HeaderMap) -> bool {
        let upgrade = headers.contains_key(header::UPGRADE);
        let safe = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
        if safe && !upgrade {
            return true;
        }
        let Some(origin) = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()) else {
            return false;
        };
        self.hosts
            .iter()
            .any(|h| origin.eq_ignore_ascii_case(&format!("http://{h}")))
    }
}

/// True unless the browser says the request comes from another site (`Sec-Fetch-Site`). Proxy
/// routes require this, so a page elsewhere can't make the user's host fetch from coordinators
/// over Tor with `<img>` or `<script>` tags. Clients that send no `Sec-Fetch-Site` pass, as do
/// direct navigations (`none`).
pub fn fetch_site_ok(headers: &HeaderMap) -> bool {
    match headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        None => true,
        Some(v) => v.eq_ignore_ascii_case("same-origin") || v.eq_ignore_ascii_case("none"),
    }
}

/// A fresh nonce for one HTML response: 18 random bytes, base64 (24 characters, no padding).
pub fn nonce() -> String {
    let mut bytes = [0u8; 18];
    getrandom::fill(&mut bytes).unwrap_or_else(|_| std::process::abort());
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The Content-Security-Policy for an HTML page served with `nonce`.
///
/// `style-src 'unsafe-inline'` is a W0 concession: the RoboSats UI (MUI/emotion) injects `<style>`
/// elements at run time; W1 passes the nonce to emotion and drops it. Scripts never run inline
/// without the nonce, connections go only to this origin, and nothing may frame the page.
pub fn csp(nonce: &str) -> String {
    [
        "default-src 'none'".to_string(),
        format!("script-src 'self' 'nonce-{nonce}' 'wasm-unsafe-eval'"),
        "style-src 'self' 'unsafe-inline'".to_string(),
        "img-src 'self' data: blob:".to_string(),
        "font-src 'self'".to_string(),
        "media-src 'self'".to_string(),
        "connect-src 'self'".to_string(),
        "worker-src 'self' blob:".to_string(),
        "manifest-src 'self'".to_string(),
        "base-uri 'none'".to_string(),
        "form-action 'none'".to_string(),
        "frame-ancestors 'none'".to_string(),
    ]
    .join("; ")
}

/// Headers every response carries.
pub fn common_headers(headers: &mut HeaderMap) {
    let set = |h: &mut HeaderMap, k: header::HeaderName, v: &'static str| {
        h.insert(k, HeaderValue::from_static(v));
    };
    set(headers, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(headers, header::REFERRER_POLICY, "no-referrer");
    set(headers, header::X_FRAME_OPTIONS, "DENY");
    headers.insert(
        "cross-origin-opener-policy",
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static(
            "camera=(), microphone=(), geolocation=(), payment=(), usb=(), serial=(), bluetooth=(), \
             hid=(), midi=(), display-capture=(), clipboard-read=(), interest-cohort=()",
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    fn allowed() -> Allowed {
        Allowed::new(vec!["127.0.0.1:12596".into(), "100.64.0.3:12596".into()])
    }

    #[test]
    fn host_must_be_ours() {
        let a = allowed();
        assert!(a.host_ok(&headers(&[("host", "127.0.0.1:12596")])));
        assert!(a.host_ok(&headers(&[("host", "100.64.0.3:12596")])));
        assert!(!a.host_ok(&headers(&[("host", "evil.example:12596")])));
        assert!(!a.host_ok(&headers(&[("host", "127.0.0.1")])));
        assert!(!a.host_ok(&HeaderMap::new()));
    }

    #[test]
    fn state_changes_and_websockets_need_our_origin() {
        let a = allowed();
        assert!(a.origin_ok(&Method::GET, &HeaderMap::new()));
        assert!(!a.origin_ok(&Method::POST, &HeaderMap::new()));
        assert!(a.origin_ok(
            &Method::POST,
            &headers(&[("origin", "http://127.0.0.1:12596")])
        ));
        assert!(!a.origin_ok(
            &Method::POST,
            &headers(&[("origin", "http://evil.example")])
        ));
        assert!(!a.origin_ok(
            &Method::POST,
            &headers(&[("origin", "https://127.0.0.1:12596")])
        ));
        assert!(!a.origin_ok(&Method::GET, &headers(&[("upgrade", "websocket")])));
        assert!(!a.origin_ok(
            &Method::GET,
            &headers(&[("upgrade", "websocket"), ("origin", "http://evil.example")])
        ));
        assert!(a.origin_ok(
            &Method::GET,
            &headers(&[
                ("upgrade", "websocket"),
                ("origin", "http://100.64.0.3:12596")
            ])
        ));
        assert!(!a.origin_ok(&Method::DELETE, &headers(&[("origin", "null")])));
    }

    #[test]
    fn cross_site_fetches_are_refused() {
        assert!(fetch_site_ok(&HeaderMap::new()));
        assert!(fetch_site_ok(&headers(&[(
            "sec-fetch-site",
            "same-origin"
        )])));
        assert!(fetch_site_ok(&headers(&[("sec-fetch-site", "none")])));
        assert!(!fetch_site_ok(&headers(&[(
            "sec-fetch-site",
            "cross-site"
        )])));
        assert!(!fetch_site_ok(&headers(&[("sec-fetch-site", "same-site")])));
    }

    #[test]
    fn nonces_are_fresh_and_csp_is_strict() {
        let (a, b) = (nonce(), nonce());
        assert_ne!(a, b);
        assert_eq!(a.len(), 24);
        let p = csp(&a);
        assert!(p.contains(&format!("'nonce-{a}'")));
        assert!(p.starts_with("default-src 'none'"));
        assert!(p.contains("connect-src 'self'"));
        assert!(p.contains("frame-ancestors 'none'"));
        assert!(!p.contains("script-src 'self' 'unsafe-inline'"));
        assert!(!p.contains("'unsafe-eval'"));
    }
}
