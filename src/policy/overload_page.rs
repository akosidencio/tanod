//! The page a browser gets when its request is shed.
//!
//! Rendered once per policy generation, never per request: a shed happens
//! exactly when the proxy is busiest, so answering one should cost a header
//! block and a clone of shared bytes.

use crate::config::schema::Overload;
use bytes::Bytes;
use http::{HeaderMap, Method};

/// A rendered page and whether it is the built-in one.
///
/// The built-in page loads nothing, so it is sent with a CSP that forbids
/// loading anything. A custom page may reference its own assets and is sent
/// without one.
#[derive(Debug, Clone)]
pub struct OverloadPage {
    pub html: Bytes,
    pub built_in: bool,
}

const BUILT_IN: &str = include_str!("overload_page.html");

/// Render the configured page, or `None` when it is turned off.
pub fn render(overload: &Overload) -> Option<OverloadPage> {
    let page = &overload.page;
    if !page.enabled {
        return None;
    }
    let (template, built_in) = match &page.template {
        Some(custom) => (custom.as_str(), false),
        None => (BUILT_IN, true),
    };
    let refresh = page.refresh.as_duration().as_secs().max(1).to_string();
    let retry_after = overload
        .retry_after
        .as_duration()
        .as_secs()
        .max(1)
        .to_string();
    // Text first and numbers last, so a placeholder written inside the
    // configured message is filled in too. Escaping cannot produce `{{`, so
    // configured text can introduce no placeholder the operator did not write.
    let html = template
        .replace("{{title}}", &escape(&page.title))
        .replace("{{message}}", &escape(&page.message))
        .replace("{{lang}}", &escape(&page.lang))
        .replace("{{refresh}}", &refresh)
        .replace("{{retry_after}}", &retry_after);
    Some(OverloadPage {
        html: Bytes::from(html),
        built_in,
    })
}

/// Is this a browser loading a page, as opposed to a script fetching data?
///
/// Only a navigation can show HTML to a person. A Next.js flight or prefetch
/// is parsed by the router, an API call by its client, and `HEAD` has no body
/// at all; all of them keep the bare status.
pub fn is_navigation(method: &Method, headers: &HeaderMap) -> bool {
    if method != Method::GET {
        return false;
    }
    if crate::classifier::nextjs::RSC_KEY_HEADERS
        .iter()
        .any(|h| headers.contains_key(*h))
    {
        return false;
    }
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    // Every current browser sends this, and it is the one signal that means
    // "a person is loading a page" rather than "a script asked for HTML".
    if let Some(mode) = header("sec-fetch-mode") {
        return mode.eq_ignore_ascii_case("navigate");
    }
    header("accept").is_some_and(|accept| accept.to_ascii_lowercase().contains("text/html"))
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn overload(yaml: &str) -> Overload {
        serde_saphyr::from_str(yaml).unwrap()
    }

    fn html(page: &OverloadPage) -> &str {
        std::str::from_utf8(&page.html).unwrap()
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (name, value) in pairs {
            h.insert(*name, HeaderValue::from_static(value));
        }
        h
    }

    #[test]
    fn the_default_page_fills_every_placeholder() {
        let page = render(&Overload::default()).unwrap();
        assert!(page.built_in);
        let html = html(&page);
        assert!(!html.contains("{{"), "unfilled placeholder in:\n{html}");
        assert!(html.contains(r#"<meta http-equiv="refresh" content="5">"#));
        assert!(html.contains("refresh by itself in 5 seconds"));
        assert!(html.contains(r#"<html lang="en">"#));
    }

    #[test]
    fn configured_text_is_escaped() {
        let page = render(&overload(
            "page:\n  title: \"<script>alert(1)</script>\"\n  message: \"Tom & Jerry's \\\"sale\\\"\"\n",
        ))
        .unwrap();
        let html = html(&page);
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("Tom &amp; Jerry&#39;s &quot;sale&quot;"));
    }

    #[test]
    fn a_message_may_use_the_number_placeholders() {
        let page = render(&overload(
            "retry_after: 3s\npage:\n  refresh: 20s\n  message: \"Back in {{refresh}}s (retry {{retry_after}}s)\"\n",
        ))
        .unwrap();
        assert!(html(&page).contains("Back in 20s (retry 3s)"));
    }

    #[test]
    fn a_custom_template_replaces_the_built_in_one() {
        let mut o = overload("page:\n  title: Sale\n");
        o.page.template = Some("<h1>{{title}}</h1><p>{{message}}</p>{{refresh}}".into());
        let page = render(&o).unwrap();
        assert!(!page.built_in);
        assert!(html(&page).starts_with("<h1>Sale</h1><p>We&#39;re letting visitors in"));
        assert!(html(&page).ends_with("will refresh by itself in 5 seconds.</p>5"));
    }

    #[test]
    fn a_disabled_page_renders_nothing() {
        assert!(render(&overload("page:\n  enabled: false\n")).is_none());
    }

    #[test]
    fn a_browser_navigation_gets_the_page() {
        let h = headers(&[("sec-fetch-mode", "navigate"), ("accept", "text/html,*/*")]);
        assert!(is_navigation(&Method::GET, &h));
        // No Fetch Metadata (an old browser, curl -H): fall back to Accept.
        assert!(is_navigation(
            &Method::GET,
            &headers(&[("accept", "Text/HTML")])
        ));
    }

    #[test]
    fn data_requests_keep_the_bare_status() {
        // A script asking for HTML is still a script.
        let fetch = headers(&[("sec-fetch-mode", "cors"), ("accept", "text/html")]);
        assert!(!is_navigation(&Method::GET, &fetch));
        // Next.js flights and prefetches are parsed by the router.
        let rsc = headers(&[("rsc", "1"), ("accept", "text/html")]);
        assert!(!is_navigation(&Method::GET, &rsc));
        let prefetch = headers(&[("next-router-prefetch", "1"), ("accept", "text/html")]);
        assert!(!is_navigation(&Method::GET, &prefetch));
        // An API client, and anything without a body.
        assert!(!is_navigation(
            &Method::GET,
            &headers(&[("accept", "application/json")])
        ));
        assert!(!is_navigation(&Method::GET, &HeaderMap::new()));
        let html = headers(&[("accept", "text/html")]);
        assert!(!is_navigation(&Method::HEAD, &html));
        assert!(!is_navigation(&Method::POST, &html));
    }
}
