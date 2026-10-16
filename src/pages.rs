//! The few responses hostd generates itself: the route index / 404 page, the 502 page
//! and terse error replies.
//!
//! Every function returns a complete HTTP/1.1 response (status line, headers, body) as
//! bytes, ready to write to the socket. All carry `Connection: close` because hostd
//! never serves a second request on a connection it answered itself.

use crate::config::Config;

/// Inline stylesheet shared by the HTML pages.
///
/// Inline so the pages need no extra request (and therefore no extra route); the
/// `prefers-color-scheme` block keeps them readable next to a dark-themed dev setup.
const STYLE: &str = "body{font:16px/1.5 system-ui,sans-serif;max-width:40rem;margin:4rem auto;\
padding:0 1rem;color:#222;background:#fff}\
@media(prefers-color-scheme:dark){body{color:#ddd;background:#161616}a{color:#8ab4f8}}\
h1{font-size:1.25rem}code{font-family:ui-monospace,monospace}\
table{border-collapse:collapse;width:100%}td{padding:.25rem .5rem .25rem 0}\
td:last-child{text-align:right;opacity:.6}";

/// Escape text for safe inclusion in HTML.
///
/// Needed because the name shown on the 404 page comes straight from the client's
/// `Host` header.
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

/// Assemble a response with the headers every hostd page shares.
///
/// `Cache-Control: no-store` stops a browser from caching an error page and then
/// showing it after the app behind the name has started.
fn response(status: u16, reason: &str, content_type: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\n\
Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Wrap `body` in a minimal HTML document.
fn html(status: u16, reason: &str, title: &str, body: &str) -> Vec<u8> {
    let doc = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>{}</title><style>{STYLE}</style></head><body>{body}</body></html>",
        escape(title)
    );
    response(status, reason, "text/html; charset=utf-8", &doc)
}

/// Plain-text reply.
fn text(status: u16, reason: &str, body: &str) -> Vec<u8> {
    response(status, reason, "text/plain; charset=utf-8", body)
}

/// The route index.
///
/// With `missing = None` this is the 200 page for the bare suffix (`localhost`); with
/// `missing = Some(name)` it is the 404 for an unconfigured name, listing what *is*
/// configured so a typo is easy to spot.
pub fn index(config: &Config, missing: Option<&str>) -> Vec<u8> {
    let mut body = String::new();
    match missing {
        Some(name) => body.push_str(&format!(
            "<h1>No route named <code>{}</code></h1>",
            escape(name)
        )),
        None => body.push_str("<h1>hostd</h1>"),
    }
    if config.routes.is_empty() {
        body.push_str("<p>No routes are configured yet. Add some under <code>[routes]</code> in the hostd config file.</p>");
    } else {
        body.push_str("<table>");
        for (name, route) in &config.routes {
            let url = escape(&config.url_for(name));
            body.push_str(&format!(
                "<tr><td><a href=\"{url}\">{}</a></td><td>:{}</td></tr>",
                escape(&config.host_for(name)),
                route.port
            ));
        }
        body.push_str("</table>");
    }
    match missing {
        Some(_) => html(404, "Not Found", "hostd: no such route", &body),
        None => html(200, "OK", "hostd", &body),
    }
}

/// 502 page for a route whose backend refused or timed out.
pub fn bad_gateway(name: &str, port: u16) -> Vec<u8> {
    let body = format!(
        "<h1>Nothing is listening for <code>{}</code></h1>\
<p>This name is routed to port <code>{port}</code>, but the connection failed. \
Start the app, or fix the port in the hostd config.</p>",
        escape(name)
    );
    html(502, "Bad Gateway", "hostd: backend unreachable", &body)
}

/// 404 for a `Host` outside our domain.
///
/// Deliberately says nothing about which routes exist: such a request is either a
/// misconfigured client or a DNS-rebinding attempt.
pub fn not_found() -> Vec<u8> {
    text(404, "Not Found", "hostd: unknown host\n")
}

/// 400 for a request head that is not valid HTTP.
pub fn bad_request() -> Vec<u8> {
    text(400, "Bad Request", "hostd: malformed request\n")
}

/// 431 for a request head over the size limit.
pub fn head_too_large() -> Vec<u8> {
    text(
        431,
        "Request Header Fields Too Large",
        "hostd: request headers too large\n",
    )
}

/// 408 for a client that did not send a request head in time.
pub fn request_timeout() -> Vec<u8> {
    text(
        408,
        "Request Timeout",
        "hostd: timed out waiting for request\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse;

    /// Split a response into (head, body) as strings.
    fn split(resp: &[u8]) -> (String, String) {
        let s = String::from_utf8(resp.to_vec()).unwrap();
        let (head, body) = s.split_once("\r\n\r\n").unwrap();
        (head.to_owned(), body.to_owned())
    }

    #[test]
    fn escape_covers_html_metacharacters() {
        assert_eq!(
            escape("<a href=\"x\">&'"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
    }

    #[test]
    fn content_length_matches_body_for_every_page() {
        let cfg = parse("[routes]\nhealth = 8808\n").unwrap();
        let pages = [
            index(&cfg, None),
            index(&cfg, Some("nope")),
            bad_gateway("health", 8808),
            not_found(),
            bad_request(),
            head_too_large(),
            request_timeout(),
        ];
        for page in pages {
            let (head, body) = split(&page);
            let declared: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(declared, body.len(), "{head}");
            assert!(head.contains("Connection: close"));
        }
    }

    #[test]
    fn index_lists_routes_with_links() {
        let cfg = parse("[routes]\nhealth = 8808\ngrafana = 3000\n").unwrap();
        let (head, body) = split(&index(&cfg, None));
        assert!(head.starts_with("HTTP/1.1 200 OK"));
        assert!(body.contains("href=\"http://health.localhost\""));
        assert!(body.contains(">grafana.localhost<"));
        assert!(body.contains(":8808"));
    }

    #[test]
    fn missing_route_is_404_and_escaped() {
        let cfg = parse("[routes]\nhealth = 8808\n").unwrap();
        let (head, body) = split(&index(&cfg, Some("<script>alert(1)</script>")));
        assert!(head.starts_with("HTTP/1.1 404"));
        assert!(!body.contains("<script>alert"));
        assert!(body.contains("&lt;script&gt;"));
        assert!(body.contains("health.localhost"));
    }

    #[test]
    fn empty_config_index_explains_itself() {
        let cfg = parse("").unwrap();
        let (_, body) = split(&index(&cfg, None));
        assert!(body.contains("No routes are configured"));
    }

    #[test]
    fn bad_gateway_names_route_and_port() {
        let (head, body) = split(&bad_gateway("vite", 5173));
        assert!(head.starts_with("HTTP/1.1 502"));
        assert!(body.contains("vite") && body.contains("5173"));
    }
}
