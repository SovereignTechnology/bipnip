//! Per-response edits of the two UI pages (`basic.html`, `pro.html`), which are this project's own
//! build output, never user content:
//! - every `<script` and `<style` tag gets the response's nonce (the page sets
//!   `window.RobosatsSettings` inline and styles its no-script notice inline; at run time the UI
//!   reads the nonce off its script tag and gives it to the styles it adds);
//! - the Django-era `onion-location` meta tag, whose value is an unrendered `{{ ONION_LOCATION }}`
//!   template variable outside Django, is removed.

/// Returns `page` with `nonce` on every `<script` and `<style` tag and without the onion-location
/// meta tag.
pub fn prepare_page(page: &str, nonce: &str) -> String {
    let mut out = String::with_capacity(page.len() + 64);
    for line in page.split_inclusive('\n') {
        if line.contains("http-equiv=\"onion-location\"") {
            continue;
        }
        out.push_str(line);
    }
    out.replace("<script", &format!("<script nonce=\"{nonce}\""))
        .replace("<style", &format!("<style nonce=\"{nonce}\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_nonce_and_drops_onion_location() {
        let page = "<html><head>\n  <meta http-equiv=\"onion-location\" content=\"{{ ONION_LOCATION }}\" />\n<script>window.RobosatsSettings='selfhosted-basic'</script>\n<script defer src=\"/static/frontend/main.js\"></script>\n<style>.noscript{display:none}</style>\n</head></html>\n";
        let out = prepare_page(page, "N0NCE");
        assert!(!out.contains("onion-location"));
        assert!(!out.contains("{{"));
        assert_eq!(out.matches("<script nonce=\"N0NCE\"").count(), 2);
        assert!(!out.contains("<script>"));
        assert_eq!(out.matches("<style nonce=\"N0NCE\"").count(), 1);
        assert!(!out.contains("<style>"));
    }
}
