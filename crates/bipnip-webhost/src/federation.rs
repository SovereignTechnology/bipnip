//! Coordinators the host may proxy to: RoboSats' `federation.json`, reduced to one onion address
//! per network and alias. Only `http://<v3 onion>` addresses are accepted, so a request for a
//! coordinator can never be turned into a request to an arbitrary host.

use std::collections::HashMap;

/// Networks the UI knows (`settings.network`).
pub const NETWORKS: [&str; 2] = ["mainnet", "testnet"];

/// Where a coordinator is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    /// The `.onion` host name (v3: 56 base32 characters + `.onion`).
    pub host: String,
    /// TCP port (80 unless the address names one).
    pub port: u16,
}

/// The coordinators of one federation file, by `(network, alias)`.
#[derive(Debug, Clone, Default)]
pub struct Coordinators {
    by_route: HashMap<(String, String), Upstream>,
}

fn is_alias(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
}

/// Parses `http://<56 base32 chars>.onion[:port][/]` into an [`Upstream`].
pub fn parse_onion_url(url: &str) -> Option<Upstream> {
    let rest = url.strip_prefix("http://")?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let (host, port) = match rest.split_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok()?),
        None => (rest, 80),
    };
    let label = host.strip_suffix(".onion")?;
    let v3 = label.len() == 56
        && label
            .bytes()
            .all(|c| c.is_ascii_lowercase() || (b'2'..=b'7').contains(&c));
    if !v3 || port == 0 {
        return None;
    }
    Some(Upstream {
        host: host.to_string(),
        port,
    })
}

impl Coordinators {
    /// Reads RoboSats' `federation.json`: `{ "<alias>": { "shortAlias", "mainnet": {"onion"},
    /// "testnet": {"onion"} } }`. Entries without a valid onion address for a network are skipped
    /// for that network; an alias that differs from its `shortAlias` or is not a plain name is
    /// rejected.
    pub fn from_federation_json(text: &str) -> Result<Self, String> {
        let root: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("federation.json: {e}"))?;
        let entries = root
            .as_object()
            .ok_or("federation.json: expected an object of coordinators")?;
        let mut by_route = HashMap::new();
        for (alias, entry) in entries {
            if !is_alias(alias) {
                return Err(format!("federation.json: bad alias {alias:?}"));
            }
            if entry.get("shortAlias").and_then(|v| v.as_str()) != Some(alias.as_str()) {
                return Err(format!(
                    "federation.json: {alias:?} has a different shortAlias"
                ));
            }
            for network in NETWORKS {
                let onion = entry
                    .get(network)
                    .and_then(|n| n.get("onion"))
                    .and_then(|o| o.as_str());
                if let Some(upstream) = onion.and_then(parse_onion_url) {
                    by_route.insert((network.to_string(), alias.clone()), upstream);
                }
            }
        }
        Ok(Coordinators { by_route })
    }

    /// The coordinator for `/<network>/<alias>/…`, if any.
    pub fn get(&self, network: &str, alias: &str) -> Option<&Upstream> {
        self.by_route.get(&(network.to_string(), alias.to_string()))
    }

    /// Number of `(network, alias)` routes.
    pub fn len(&self) -> usize {
        self.by_route.len()
    }

    /// True when no coordinator has a usable onion address.
    pub fn is_empty(&self) -> bool {
        self.by_route.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONION: &str = "abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx";

    #[test]
    fn parses_v3_onion_urls_only() {
        assert_eq!(
            parse_onion_url(&format!("http://{ONION}.onion")),
            Some(Upstream {
                host: format!("{ONION}.onion"),
                port: 80
            })
        );
        assert_eq!(
            parse_onion_url(&format!("http://{ONION}.onion:8080/")).map(|u| u.port),
            Some(8080)
        );
        assert!(parse_onion_url(&format!("https://{ONION}.onion")).is_none());
        assert!(parse_onion_url("http://example.com").is_none());
        assert!(parse_onion_url("http://short.onion").is_none());
        assert!(parse_onion_url(&format!("http://{}.onion", ONION.to_uppercase())).is_none());
        assert!(parse_onion_url(&format!("http://{ONION}.onion/path")).is_none());
        assert!(parse_onion_url(&format!("http://{ONION}.onion:0")).is_none());
        assert!(parse_onion_url(&format!("http://user@{ONION}.onion")).is_none());
    }

    #[test]
    fn reads_federation_shape() {
        let json = format!(
            r#"{{
              "temple": {{"shortAlias": "temple",
                         "mainnet": {{"onion": "http://{ONION}.onion", "clearnet": "https://x"}},
                         "testnet": {{"onion": null}}}},
              "lake": {{"shortAlias": "lake",
                       "mainnet": {{"onion": "https://not-allowed.example"}},
                       "testnet": {{"onion": "http://{ONION}.onion"}}}}
            }}"#
        );
        let c = Coordinators::from_federation_json(&json).expect("parses");
        assert_eq!(c.len(), 2);
        assert!(c.get("mainnet", "temple").is_some());
        assert!(c.get("testnet", "temple").is_none());
        assert!(
            c.get("mainnet", "lake").is_none(),
            "non-onion address skipped"
        );
        assert!(c.get("testnet", "lake").is_some());
        assert!(c.get("regtest", "temple").is_none());
    }

    #[test]
    fn rejects_mismatched_or_odd_aliases() {
        let bad = r#"{"temple": {"shortAlias": "other", "mainnet": {}}}"#;
        assert!(Coordinators::from_federation_json(bad).is_err());
        let odd = r#"{"../x": {"shortAlias": "../x"}}"#;
        assert!(Coordinators::from_federation_json(odd).is_err());
    }
}
