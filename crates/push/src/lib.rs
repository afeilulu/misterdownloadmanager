//! The push listener (#1512): a browser userscript's pushed download, its address with the
//! request headers the site needs (a netdisk's signed links want a particular User-Agent, the
//! page's Referer and its cookies).
//!
//! The listener listens on `127.0.0.1` (default port [`PORT`]), this computer only, and is off
//! unless the user turns it on (Options → Push). Any local process can post to it, so:
//!
//! - a post is taken only with `Content-Type: application/json` (else 415): a web page's
//!   cross-origin form post can't send that header without a CORS preflight, and the preflight
//!   is answered with no `Access-Control-Allow-Origin` on purpose (unlike Click'n'Load's
//!   listener, whose posts are only ever shown), so the browser refuses to send the post at
//!   all. Tampermonkey's `GM_xmlhttpRequest` doesn't follow the page's CORS and gets through;
//! - the `Host` header must name this computer (else 403), so a page can't reach the listener
//!   through a name it resolved elsewhere (DNS rebinding);
//! - the first post from a site asks whether that site may push (Allow, Block, Not now), in
//!   the app ([`crate`]'s caller keeps the answers; the site is the browser's `Origin`, else
//!   its `Referer`, else the page address the post names — the post is taken only under the
//!   checks above, so a page can't slip a name past them);
//! - the listener takes at most [`PUSHES_PER_MINUTE`] posts a minute from each site, at most
//!   [`SITES_COUNTED`] sites a minute, a body of at most [`MAX_BODY`] bytes, and short fields;
//! - an address on this computer or its network is refused (see [`internal`]), as is anything
//!   that isn't http or https: what arrives is shown (Download File Info) or started (only
//!   when the push says `silent`), so it is checked before it is fetched.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};

/// The default port; Options can change it.
pub const PORT: u16 = 16872;
/// The most a post may be: an address, a page, cookies and a few headers fit many times over.
pub const MAX_BODY: usize = 64 * 1024;
/// The most characters of an address, a file name, a page address, an origin or a User-Agent.
pub const MAX_TEXT: usize = 2048;
/// The most characters of a post's cookies.
pub const MAX_COOKIES: usize = 8192;
/// The most extra headers one post may bring.
pub const MAX_EXTRA: usize = 16;
/// The most characters of an extra header's name and value.
pub const MAX_NAME: usize = 128;
pub const MAX_VALUE: usize = 4096;
/// Posts taken a minute from one site; more are refused until the minute is over.
pub const PUSHES_PER_MINUTE: usize = 10;
/// The most sites counted at once: a post from one more is refused until a counted site's
/// minute is over.
pub const SITES_COUNTED: usize = 100;

/// A pushed download, and the site it came from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Push {
    /// The address to download.
    pub url: String,
    /// The file name the page gave, if any.
    pub name: Option<String>,
    /// The web page the download was started from.
    pub referer: Option<String>,
    /// The origin the page's player or server answers (a netdisk's API page, most often).
    pub origin: Option<String>,
    /// The page's cookies, as one line ("k=v; …").
    pub cookies: Option<String>,
    /// The User-Agent to download with.
    pub user_agent: Option<String>,
    /// Other headers the site needs (an Authorization, an app's own), each a name and a value.
    pub extra_headers: Vec<(String, String)>,
    /// Start it at once, no Download File Info.
    pub silent: bool,
    /// Add it to the main queue, don't start.
    pub later: bool,
    /// The size the page knew, so the app needn't look it up.
    pub size: Option<u64>,
    /// The site the post came from: what the browser named, else what the post names.
    pub site: Option<String>,
}

/// What a post is as it travelled: the JSON body of a `POST /push`.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct Wire {
    url: String,
    name: Option<String>,
    referer: Option<String>,
    origin: Option<String>,
    cookies: Option<String>,
    user_agent: Option<String>,
    extra_headers: Vec<(String, String)>,
    silent: bool,
    later: bool,
    size: Option<u64>,
}

/// Called for each post taken, off the async threads: whether the app took it.
pub type OnPush = Arc<dyn Fn(Push) -> bool + Send + Sync>;

/// Why a post is refused: whether its address itself was (an internal one is forbidden, not
/// just unreadable), and the text for the answer.
#[derive(Debug)]
enum Rejected {
    /// A post that can't be taken as it is: bad JSON, an address that isn't http(s), a field
    /// too long or a header too many.
    Bad(&'static str),
    /// An address on this computer or its network.
    Internal,
}

/// One text field of a post, trimmed; empty becomes None, longer than `max` refused.
fn capped(s: &Option<String>, max: usize) -> Result<Option<String>, Rejected> {
    s.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.chars().count() > max {
                Err(Rejected::Bad("failed: a field is too long"))
            } else {
                Ok(s.to_owned())
            }
        })
        .transpose()
}

/// The body of a post as a [`Push`], or why not.
fn read(body: &[u8]) -> Result<Push, Rejected> {
    let w: Wire = serde_json::from_slice(body)
        .map_err(|_| Rejected::Bad("failed: the post can't be read"))?;
    let url = w.url.trim().to_owned();
    if url.chars().count() > MAX_TEXT {
        return Err(Rejected::Bad("failed: the address is too long"));
    }
    let parsed =
        url::Url::parse(&url).map_err(|_| Rejected::Bad("failed: the address can't be read"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(Rejected::Bad("failed: the address must be http or https"));
    }
    if internal(&parsed) {
        return Err(Rejected::Internal);
    }
    let cookies = capped(&w.cookies, MAX_COOKIES)?;
    if w.extra_headers.len() > MAX_EXTRA {
        return Err(Rejected::Bad("failed: too many headers"));
    }
    let mut extra_headers = Vec::new();
    for (n, v) in &w.extra_headers {
        let (n, v) = (n.trim(), v.trim());
        if n.is_empty() {
            continue;
        }
        if n.chars().count() > MAX_NAME || v.chars().count() > MAX_VALUE {
            return Err(Rejected::Bad("failed: a header is too long"));
        }
        extra_headers.push((n.to_owned(), v.to_owned()));
    }
    Ok(Push {
        url,
        name: capped(&w.name, MAX_TEXT)?,
        referer: capped(&w.referer, MAX_TEXT)?,
        origin: capped(&w.origin, MAX_TEXT)?,
        cookies,
        user_agent: capped(&w.user_agent, MAX_TEXT)?,
        extra_headers,
        silent: w.silent,
        later: w.later,
        size: w.size,
        site: None,
    })
}

/// A web address's host, lower case.
fn host(address: &str) -> Option<String> {
    url::Url::parse(address)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
}

/// The site a post came from: the browser's `Origin` (an `Origin` that names no web site —
/// `null`, a sandboxed frame — means no site, whatever else says), else its `Referer`, else
/// the page address the post names. The browser's own headers win when they name anything:
/// the post's body is the page's to write.
fn site(headers: &HeaderMap, push: &Push) -> Option<String> {
    let header = |name| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(host)
    };
    let origin = headers
        .get(header::ORIGIN)
        .map(|v| v.to_str().ok().and_then(host));
    match origin {
        Some(host) => host,
        None => header(header::REFERER)
            .or_else(|| push.referer.as_deref().and_then(host))
            .or_else(|| push.origin.as_deref().and_then(host)),
    }
}

/// Whether a request's `Host` names this computer (`127.0.0.1`, `localhost`, `[::1]`, with or
/// without its port): a page that reached the listener through a name it resolved itself is
/// turned away.
fn local_host(headers: &HeaderMap) -> bool {
    let named = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let named = match named.rsplit_once(':') {
        // The port, but not an IPv6 address's last group ("[::1]" keeps its colon).
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => named,
    };
    matches!(
        named.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "[::1]"
    )
}

/// Whether an address is on this computer or its own network, by its host as written: a
/// loopback, private, link-local, shared (100.64/10), unspecified, broadcast or multicast IP
/// address (IPv4, IPv6, or IPv4 inside IPv6), `localhost`, a name without a dot, or a name
/// under `.localhost`, `.local`, `.internal`, `.lan`, `.home.arpa` or `.intranet`. The `url`
/// crate reads every spelling of an IPv4 address (`2130706433`, `0x7f.1`) as the address it
/// is. (mdm-cnl keeps the same check for Click'n'Load's links.)
fn internal(u: &url::Url) -> bool {
    use std::net::{Ipv4Addr, Ipv6Addr};
    fn v4(a: Ipv4Addr) -> bool {
        let o = a.octets();
        a.is_loopback()
            || a.is_private()
            || a.is_link_local()
            || a.is_unspecified()
            || a.is_broadcast()
            || a.is_multicast()
            || o[0] == 0
            || (o[0] == 100 && o[1] & 0xc0 == 64)
            || (o[0] == 192 && o[1] == 0 && o[2] == 0)
            || (o[0] == 198 && o[1] & 0xfe == 18)
            || o[0] >= 240
    }
    fn v6(a: Ipv6Addr) -> bool {
        let s = a.segments();
        // IPv4 inside IPv6: mapped (::ffff:a.b.c.d), compatible (::a.b.c.d), NAT64 (64:ff9b::).
        let embedded = (s[..5] == [0; 5] && (s[5] == 0xffff || s[5] == 0))
            || (s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0; 4]);
        if embedded && !a.is_unspecified() && !a.is_loopback() {
            return v4(Ipv4Addr::new(
                (s[6] >> 8) as u8,
                s[6] as u8,
                (s[7] >> 8) as u8,
                s[7] as u8,
            ));
        }
        a.is_loopback()
            || a.is_unspecified()
            || a.is_multicast()
            || s[0] & 0xffc0 == 0xfe80
            || s[0] & 0xfe00 == 0xfc00
    }
    match u.host() {
        Some(url::Host::Ipv4(a)) => v4(a),
        Some(url::Host::Ipv6(a)) => v6(a),
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            !d.contains('.')
                || [
                    ".localhost",
                    ".local",
                    ".internal",
                    ".lan",
                    ".home.arpa",
                    ".intranet",
                ]
                .iter()
                .any(|end| d.ends_with(end))
        }
        None => true,
    }
}

/// Each site's posts of the last minute, to keep them under [`PUSHES_PER_MINUTE`] (None: the
/// posts that name no site, together).
#[derive(Default)]
struct Rate(Mutex<HashMap<Option<String>, VecDeque<Instant>>>);

impl Rate {
    /// One more post from `site` now, if its minute has room.
    fn take(&self, site: &Option<String>, now: Instant) -> bool {
        let mut sites = self.0.lock().unwrap();
        // What is older than a minute is forgotten, and a site with nothing left with it.
        sites.retain(|_, sent| {
            while sent
                .front()
                .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
            {
                sent.pop_front();
            }
            !sent.is_empty()
        });
        if !sites.contains_key(site) && sites.len() >= SITES_COUNTED {
            return false;
        }
        let sent = sites.entry(site.clone()).or_default();
        if sent.len() >= PUSHES_PER_MINUTE {
            return false;
        }
        sent.push_back(now);
        true
    }
}

/// The server, while kept: dropping it stops listening.
pub struct Listener {
    task: tokio::task::JoinHandle<()>,
    pub addr: SocketAddr,
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Listener {
    /// Listen on `addr` (`127.0.0.1:16872`; tests: port 0) on `rt`. An error when it can't,
    /// such as the port being taken: nothing listens then.
    pub fn start(
        rt: &tokio::runtime::Handle,
        addr: SocketAddr,
        on_push: OnPush,
    ) -> std::io::Result<Listener> {
        let std = std::net::TcpListener::bind(addr)?;
        std.set_nonblocking(true)?;
        let addr = std.local_addr()?;
        let _in = rt.enter();
        let listener = tokio::net::TcpListener::from_std(std)?;
        let app = router(on_push);
        let task = rt.spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::warn!("Push stopped: {e}");
            }
        });
        tracing::info!("Push listening on {addr}");
        Ok(Listener { task, addr })
    }
}

fn router(on_push: OnPush) -> Router {
    let rate = Arc::new(Rate::default());
    let handler = move |headers: HeaderMap, body: Bytes| {
        let (on_push, rate) = (on_push.clone(), rate.clone());
        async move {
            if !local_host(&headers) {
                return (
                    StatusCode::FORBIDDEN,
                    axum::Json(serde_json::json!({"ok": false, "error": "local only"})),
                )
                    .into_response();
            }
            let named = headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split(';').next())
                .map(str::trim);
            if named.map(|c| c.eq_ignore_ascii_case("application/json")) != Some(true) {
                return (
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    axum::Json(serde_json::json!({
                        "ok": false,
                        "error": "the body must be JSON (Content-Type: application/json)",
                    })),
                )
                    .into_response();
            }
            let mut push = match read(&body) {
                Ok(p) => p,
                Err(Rejected::Bad(why)) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        axum::Json(serde_json::json!({"ok": false, "error": why})),
                    )
                        .into_response();
                }
                Err(Rejected::Internal) => {
                    return (
                        StatusCode::FORBIDDEN,
                        axum::Json(serde_json::json!({
                            "ok": false,
                            "error": "the address is on this computer or its network",
                        })),
                    )
                        .into_response();
                }
            };
            let site = site(&headers, &push);
            push.site = site.clone();
            if !rate.take(&site, Instant::now()) {
                tracing::info!("Push: a post refused, too many this minute");
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    axum::Json(serde_json::json!({
                        "ok": false,
                        "error": "too many, try again in a minute",
                    })),
                )
                    .into_response();
            }
            let took = tokio::task::spawn_blocking(move || on_push(push))
                .await
                .unwrap_or(false);
            if took {
                axum::Json(serde_json::json!({"ok": true})).into_response()
            } else {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    axum::Json(serde_json::json!({"ok": false, "error": "not taken"})),
                )
                    .into_response()
            }
        }
    };
    Router::new()
        .route("/push", post(handler))
        .route("/ping", get(ping))
        .fallback(any(other))
        .layer(DefaultBodyLimit::max(MAX_BODY))
}

/// The script's test asks whether a listener of ours is here; say so, with the version.
async fn ping() -> Response {
    axum::Json(serde_json::json!({
        "ok": true,
        "app": "mdm",
        "version": env!("CARGO_PKG_VERSION"),
    }))
    .into_response()
}

/// A browser's preflight is answered bare on purpose (see the module doc): without an
/// `Access-Control-Allow-Origin` the browser refuses to send the post it asks about. Anything
/// else isn't here.
async fn other(method: Method) -> Response {
    if method == Method::OPTIONS {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(url: &str) -> String {
        json!({"url": url}).to_string()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_script_pushes_a_download() {
        let got: Arc<Mutex<Vec<Push>>> = Arc::default();
        let seen = got.clone();
        let l = Listener::start(
            &tokio::runtime::Handle::current(),
            "127.0.0.1:0".parse().unwrap(),
            Arc::new(move |p| {
                seen.lock().unwrap().push(p);
                true
            }),
        )
        .unwrap();
        let base = format!("http://{}", l.addr);
        let c = reqwest::Client::new();
        let r = c
            .post(format!("{base}/push"))
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ORIGIN, "https://pan.example")
            .body(
                json!({
                    "url": "https://dl.example/file.zip",
                    "name": "file.zip",
                    "referer": "https://pan.example/s/list",
                    "origin": "https://pan.example",
                    "cookies": "a=1; b=2",
                    "user_agent": "UA/1.0",
                    "extra_headers": [["Authorization", "Bearer x"], ["platform", "web"]],
                    "silent": true,
                    "size": 1234,
                })
                .to_string(),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&r.text().await.unwrap()).unwrap();
        assert_eq!(v["ok"], true);
        let got = got.lock().unwrap().clone();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].url, "https://dl.example/file.zip");
        assert_eq!(got[0].name.as_deref(), Some("file.zip"));
        assert_eq!(got[0].cookies.as_deref(), Some("a=1; b=2"));
        assert_eq!(got[0].user_agent.as_deref(), Some("UA/1.0"));
        assert_eq!(
            got[0].extra_headers,
            vec![
                ("Authorization".to_owned(), "Bearer x".to_owned()),
                ("platform".to_owned(), "web".to_owned()),
            ]
        );
        assert!(got[0].silent && !got[0].later);
        assert_eq!(got[0].size, Some(1234));
        // The browser's Origin names the site.
        assert_eq!(got[0].site.as_deref(), Some("pan.example"));
        // The test button's question.
        let r = c.get(format!("{base}/ping")).send().await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&r.text().await.unwrap()).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["app"], "mdm");
        drop(l);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_browser_cannot_send_its_post_past_the_checks() {
        let got: Arc<Mutex<Vec<Push>>> = Arc::default();
        let seen = got.clone();
        let l = Listener::start(
            &tokio::runtime::Handle::current(),
            "127.0.0.1:0".parse().unwrap(),
            Arc::new(move |p| {
                seen.lock().unwrap().push(p);
                true
            }),
        )
        .unwrap();
        let base = format!("http://{}", l.addr);
        let c = reqwest::Client::new();
        // A preflight is answered bare: no CORS header, so the browser won't send the post.
        let r = c
            .request(reqwest::Method::OPTIONS, &base)
            .header(header::ORIGIN, "https://evil.example")
            .header("Access-Control-Request-Method", "POST")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert!(r.headers().get("access-control-allow-origin").is_none());
        assert!(
            r.headers()
                .get("access-control-allow-private-network")
                .is_none()
        );
        // A form post (no CORS header of its own possible without a preflight): 415.
        let r = c
            .post(format!("{base}/push"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body("url=https%3A%2F%2Fa.example%2F1")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        // A post naming another host (DNS rebinding): 403.
        let r = c
            .post(format!("{base}/push"))
            .header(header::HOST, "evil.example")
            .header(header::CONTENT_TYPE, "application/json")
            .body(body("https://a.example/1"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        // And the checks the address itself must pass.
        let send = |u: &'static str| {
            let c = c.clone();
            let base = base.clone();
            async move {
                c.post(format!("{base}/push"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(body(u))
                    .send()
                    .await
                    .unwrap()
            }
        };
        assert_eq!(
            send("ftp://a.example/1").await.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            send("http://192.168.1.1/").await.status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            send("http://localhost/admin").await.status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(send("not a link").await.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            send("").await.status(),
            StatusCode::BAD_REQUEST,
            "an empty body"
        );
        assert!(got.lock().unwrap().is_empty());
        drop(l);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn posts_are_capped() {
        let l = Listener::start(
            &tokio::runtime::Handle::current(),
            "127.0.0.1:0".parse().unwrap(),
            Arc::new(|_| true),
        )
        .unwrap();
        let base = format!("http://{}", l.addr);
        let c = reqwest::Client::new();
        let send = |v: serde_json::Value| {
            let c = c.clone();
            let base = base.clone();
            async move {
                c.post(format!("{base}/push"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ORIGIN, "https://cap.example")
                    .body(v.to_string())
                    .send()
                    .await
                    .unwrap()
            }
        };
        // More headers than a post may bring.
        let many: Vec<Vec<String>> = (0..=MAX_EXTRA)
            .map(|i| vec![format!("x-{i}"), "v".into()])
            .collect();
        assert_eq!(
            send(json!({"url": "https://a.example/1", "extra_headers": many}))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        // Cookies longer than a post may bring (and the whole body over the limit: 413).
        let long = "a".repeat(MAX_COOKIES + 1);
        assert_eq!(
            send(json!({"url": "https://a.example/1", "cookies": long}))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        let big = format!("url=https%3A%2F%2Fa.example%2F{}", "a".repeat(MAX_BODY));
        assert_eq!(
            c.post(format!("{base}/push"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(big)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        // Unknown fields are ignored.
        assert_eq!(
            send(json!({"url": "https://a.example/2", "spoiler": 1}))
                .await
                .status(),
            StatusCode::OK
        );
        drop(l);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_flood_is_refused() {
        let n = Arc::new(Mutex::new(0usize));
        let seen = n.clone();
        let l = Listener::start(
            &tokio::runtime::Handle::current(),
            "127.0.0.1:0".parse().unwrap(),
            Arc::new(move |_| {
                *seen.lock().unwrap() += 1;
                true
            }),
        )
        .unwrap();
        let c = reqwest::Client::new();
        let url = format!("http://{}/push", l.addr);
        let mut refused = 0;
        for i in 0..PUSHES_PER_MINUTE + 5 {
            let r = c
                .post(&url)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ORIGIN, "https://flood.example")
                .body(body(&format!("https://a.example/{i}")))
                .send()
                .await
                .unwrap();
            if r.status() == StatusCode::TOO_MANY_REQUESTS {
                refused += 1;
            }
        }
        assert_eq!((*n.lock().unwrap(), refused), (PUSHES_PER_MINUTE, 5));
        drop(l);
    }

    #[test]
    fn a_taken_port_says_so() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let first = Listener::start(
            rt.handle(),
            "127.0.0.1:0".parse().unwrap(),
            Arc::new(|_| true),
        )
        .unwrap();
        assert!(Listener::start(rt.handle(), first.addr, Arc::new(|_| true)).is_err());
    }

    #[test]
    fn the_post_its_own_page_names_the_site_when_the_browser_names_none() {
        let mut h = HeaderMap::new();
        let p = |referer: Option<&str>| Push {
            referer: referer.map(str::to_owned),
            origin: Some("https://api.example".to_owned()),
            ..Default::default()
        };
        // The browser names nothing (a userscript's request often has neither header): the
        // page the post names stands in.
        assert_eq!(
            site(&h, &p(Some("https://Page.Example/s"))).as_deref(),
            Some("page.example")
        );
        // No page either: the origin the post names, last.
        assert_eq!(site(&h, &p(None)).as_deref(), Some("api.example"));
        h.insert(
            header::REFERER,
            "https://FromBrowser.example/list".parse().unwrap(),
        );
        assert_eq!(
            site(&h, &p(Some("https://in-body.example/s"))).as_deref(),
            Some("frombrowser.example")
        );
        h.insert(header::ORIGIN, "https://links.example".parse().unwrap());
        assert_eq!(site(&h, &p(None)).as_deref(), Some("links.example"));
        // A sandboxed frame's "null" origin: no site, whatever else says.
        h.insert(header::ORIGIN, "null".parse().unwrap());
        h.remove(header::REFERER);
        assert_eq!(site(&h, &p(Some("https://in-body.example/s"))), None);
    }

    #[test]
    fn the_listener_answers_only_on_this_computers_name() {
        let mut h = HeaderMap::new();
        assert!(!local_host(&h));
        for good in [
            "127.0.0.1:16872",
            "localhost",
            "LOCALHOST:80",
            "[::1]",
            "[::1]:16872",
        ] {
            h.insert(header::HOST, good.parse().unwrap());
            assert!(local_host(&h), "{good}");
        }
        for bad in [
            "evil.example",
            "127.0.0.1.evil.example",
            "[fe80::1]",
            "127.0.0.1.evil.example:80",
        ] {
            h.insert(header::HOST, bad.parse().unwrap());
            assert!(!local_host(&h), "{bad}");
        }
    }

    #[test]
    fn a_post_is_read_with_its_defaults() {
        let p = read(body("https://a.example/1").as_bytes()).unwrap();
        assert_eq!(p.url, "https://a.example/1");
        assert_eq!(p.name, None);
        assert!(p.extra_headers.is_empty());
        assert!(!p.silent && !p.later);
        assert_eq!(p.site, None);
        // An empty name and an empty header name are trimmed away; a too-long field or header
        // is refused, not cut short.
        let p = read(
            json!({
                "url": "  https://a.example/2  ",
                "name": "  ",
                "extra_headers": [["", "v"], ["x-ok", "v"]],
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(p.url, "https://a.example/2");
        assert_eq!(p.name, None);
        assert_eq!(p.extra_headers, vec![("x-ok".to_owned(), "v".to_owned())]);
        let long = "v".repeat(MAX_VALUE + 1);
        assert!(
            read(
                json!({"url": "https://a.example/3", "extra_headers": [["x-ok", long]]})
                    .to_string()
                    .as_bytes()
            )
            .is_err()
        );
        // The internal check, through read.
        assert!(read(body("http://127.0.0.1/x").as_bytes()).is_err());
        assert!(read(body("https://nas.local/share").as_bytes()).is_err());
        assert!(read(body("https://a.example/4").as_bytes()).is_ok());
    }
}
