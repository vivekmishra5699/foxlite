//! Turn address-bar text into a URL: full URLs and bare domains navigate
//! directly; anything else becomes a web search with the chosen engine.
//! Also: internal-page detection (is this URL one of our bundled pages?).

use std::net::IpAddr;

use url::Url;

/// Internal home/new-tab page (bundled, loaded via `WebviewUrl::App`).
pub const NEW_TAB_PAGE: &str = "newtab.html";

/// Our bundled internal pages.
pub const INTERNAL_PAGES: [&str; 4] =
    [NEW_TAB_PAGE, "settings.html", "history.html", "source.html"];

/// Search-URL template for an engine id; `{q}` is replaced with the query.
fn search_template(engine: &str) -> &'static str {
    match engine {
        "google" => "https://www.google.com/search?q={q}",
        "bing" => "https://www.bing.com/search?q={q}",
        "brave" => "https://search.brave.com/search?q={q}",
        "ecosia" => "https://www.ecosia.org/search?q={q}",
        "startpage" => "https://www.startpage.com/sp/search?query={q}",
        "yahoo" => "https://search.yahoo.com/search?p={q}",
        _ => "https://duckduckgo.com/?q={q}",
    }
}

/// Does `s` (no scheme, no whitespace) look like something the user means as a
/// host — `example.com`, `sub.example.co.uk/path`, `localhost:8080`,
/// `192.168.1.1`, `[::1]` — rather than a search like `1.5`, `e.g.` or `v2.0`?
fn looks_like_host(s: &str) -> bool {
    let authority = s.split(['/', '?', '#']).next().unwrap_or(s);
    // Strip a port, unless it's a bracketed IPv6 literal.
    let host = if authority.starts_with('[') {
        return authority.contains(']');
    } else {
        authority.rsplit_once(':').map_or(authority, |(h, port)| {
            if port.chars().all(|c| c.is_ascii_digit()) {
                h
            } else {
                authority
            }
        })
    };
    if host.eq_ignore_ascii_case("localhost") || host.parse::<IpAddr>().is_ok() {
        return true;
    }
    // Every label non-empty and made of URL-safe host characters, and the last
    // label (the TLD) is at least two letters — so "1.5" and "e.g." search.
    let mut labels = host.split('.');
    let Some(tld) = labels.next_back() else {
        return false;
    };
    let tld_ok = tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic());
    let mut rest = labels.peekable();
    let has_rest = rest.peek().is_some();
    tld_ok
        && has_rest
        && rest.all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

/// Scheme for a bare host: plain `http` for loopback / private addresses (dev
/// servers rarely have TLS), `https` for everything else.
fn scheme_for(host: &str) -> &'static str {
    let bare = host.split([':', '/']).next().unwrap_or(host);
    let private = bare.eq_ignore_ascii_case("localhost")
        || bare
            .parse::<IpAddr>()
            .map(|ip| match ip {
                IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
                IpAddr::V6(v6) => v6.is_loopback(),
            })
            .unwrap_or(false);
    if private {
        "http"
    } else {
        "https"
    }
}

/// Parse address-bar `input` into a URL, using `engine` for searches.
pub fn to_url(input: &str, engine: &str) -> Option<Url> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }

    // Already a full URL with a web scheme.
    if let Ok(url) = Url::parse(s) {
        if matches!(url.scheme(), "http" | "https" | "file" | "about" | "data") {
            return Some(url);
        }
    }

    // Bare host like "example.com/path" or "localhost:8080".
    if !s.contains(char::is_whitespace) && looks_like_host(s) {
        if let Ok(url) = Url::parse(&format!("{}://{s}", scheme_for(s))) {
            return Some(url);
        }
    }

    // Otherwise: search.
    let query: String = url::form_urlencoded::byte_serialize(s.as_bytes()).collect();
    Url::parse(&search_template(engine).replace("{q}", &query)).ok()
}

/// Is this URL one of our bundled internal pages rather than real web content?
/// Internal pages are served from the app origin only (`tauri://localhost` on
/// macOS/Linux, `http://tauri.localhost` on Windows) — an external page whose
/// path happens to end in `settings.html` is NOT internal.
pub fn is_internal(url: &str) -> bool {
    url.is_empty() || app_path(url).is_some()
}

/// Path component of `url` if (and only if) it is on the app origin.
fn app_path(url: &str) -> Option<String> {
    let u = Url::parse(url).ok()?;
    let local = matches!(
        (u.scheme(), u.host_str()),
        ("tauri", Some("localhost"))
            | ("http", Some("tauri.localhost"))
            | ("https", Some("tauri.localhost"))
    );
    local.then(|| u.path().to_string())
}

/// Which bundled page (e.g. "settings.html") an internal URL points at, ignoring
/// query/fragment. `None` for anything else (including all external URLs).
pub fn page_of(url: &str) -> Option<&'static str> {
    let path = app_path(url)?;
    let name = path.rsplit('/').next().unwrap_or("");
    INTERNAL_PAGES.into_iter().find(|p| *p == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nav(s: &str) -> String {
        to_url(s, "duckduckgo").unwrap().to_string()
    }

    #[test]
    fn full_urls_pass_through() {
        assert_eq!(
            nav("https://example.com/a?b=1"),
            "https://example.com/a?b=1"
        );
        assert_eq!(nav("http://localhost:3000/x"), "http://localhost:3000/x");
    }

    #[test]
    fn bare_hosts_navigate() {
        assert_eq!(nav("example.com"), "https://example.com/");
        assert_eq!(
            nav("sub.example.co.uk/path"),
            "https://sub.example.co.uk/path"
        );
        assert_eq!(nav("localhost:8080"), "http://localhost:8080/");
        assert_eq!(nav("192.168.1.10/admin"), "http://192.168.1.10/admin");
        assert_eq!(nav("1.1.1.1"), "https://1.1.1.1/");
    }

    #[test]
    fn dotted_non_hosts_search() {
        for s in ["1.5", "e.g.", "v2.0", "node.js is fun", "3.14 pi", "a."] {
            assert!(nav(s).starts_with("https://duckduckgo.com/?q="), "{s}");
        }
        assert_eq!(nav("hello world"), "https://duckduckgo.com/?q=hello+world");
    }

    #[test]
    fn engines() {
        assert!(to_url("cats", "google")
            .unwrap()
            .as_str()
            .starts_with("https://www.google.com/"));
        assert!(to_url("cats", "nope")
            .unwrap()
            .as_str()
            .starts_with("https://duckduckgo.com/"));
    }

    #[test]
    fn internal_only_on_app_origin() {
        assert!(is_internal(""));
        assert!(is_internal("tauri://localhost/settings.html?x=1"));
        assert!(is_internal("http://tauri.localhost/history.html"));
        assert!(!is_internal("https://example.com/settings.html"));
        assert!(!is_internal("https://example.com/?ref=tauri.localhost"));
        assert!(!is_internal("https://tauri.localhost.evil.com/newtab.html"));
        assert_eq!(
            page_of("tauri://localhost/settings.html#a"),
            Some("settings.html")
        );
        assert_eq!(
            page_of("tauri://localhost/newtab.html?incognito=1"),
            Some("newtab.html")
        );
        assert_eq!(page_of("https://example.com/settings.html"), None);
        assert_eq!(page_of("tauri://localhost/other.html"), None);
    }
}
