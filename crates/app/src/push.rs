//! The push listener in the app (`mdm_push`, #1512): the downloads a browser userscript pushes
//! while Options → Push → "Accept pushes from browser userscripts" is on.
//!
//! Any local program can post to the listener, so:
//! - the first push from a site asks whether that site may push (Allow, Block, Not now); the
//!   answer is kept for the site (Options' `push_sites`), "Not now" only for those pushes;
//! - what an allowed site sends is a download like one handed over by the browser: shown
//!   (Download File Info) before it starts, or started at once when the push says `silent`;
//! - the listener takes at most `mdm_push::PUSHES_PER_MINUTE` pushes a minute from each site,
//!   and at most [`WAITING`] wait here for an answer; the window asks one site at a time, and
//!   a site brings MDM's window up at most once a minute ([`RAISE_EVERY`]);
//! - the site is the browser's `Origin`, else its `Referer`, else the page address the push
//!   names (nothing arrives that a web page could send past the listener's own checks:
//!   `mdm_push`); links to this computer or its network never arrive.
//!
//! The listener runs off the window's thread; what arrives waits in an inbox the window
//! empties each frame (`App::poll_push`).

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mdm_ipc as ipc;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::app::RequestParts;
use crate::dialogs::{Outcome, window};
use crate::winui::{self, ButtonKind, LIGHT_BG, LIGHT_TEXT};

/// The most pushes waiting for the window: the inbox and those waiting for the site's answer.
pub const WAITING: usize = 20;
/// How often one site's pushes bring MDM's window up: its downloads still arrive in between,
/// without the window coming to the front again.
pub const RAISE_EVERY: Duration = Duration::from_secs(60);

/// The header names a push may not set (each has its own field, or is the download's own):
/// `mdm_ipc::Command::extra_headers` documents the list.
const RESERVED: [&str; 19] = [
    "host",
    "content-length",
    "content-type",
    "transfer-encoding",
    "te",
    "connection",
    "upgrade",
    "expect",
    "range",
    "accept-encoding",
    "if-match",
    "if-none-match",
    "if-modified-since",
    "if-unmodified-since",
    "if-range",
    "cookie",
    "referer",
    "origin",
    "user-agent",
];

fn reserved(name: &str) -> bool {
    RESERVED.contains(&name.to_ascii_lowercase().as_str())
}

/// A push's extra headers in the download's requests: the readable, unreserved ones.
pub(crate) fn apply_extra_headers(headers: &mut HeaderMap, context: &ipc::Command) {
    for (name, value) in &context.extra_headers {
        if reserved(name) {
            continue;
        }
        let (Ok(name), Ok(value)) = (HeaderName::from_str(name), HeaderValue::from_str(value))
        else {
            continue;
        };
        headers.insert(name, value);
    }
}

/// A push's extra headers out again: Download File Info clears them with the site's cookies
/// and Referer when its address is edited to another site's.
pub(crate) fn remove_extra_headers(request: &mut RequestParts, context: &ipc::Command) {
    for (name, _) in &context.extra_headers {
        if reserved(name) {
            continue;
        }
        if let Ok(name) = HeaderName::from_str(name) {
            request.headers.remove(&name);
        }
    }
}

/// The listener, while on, the inbox, and the pushes waiting for their site's answer.
pub struct Inbox {
    rt: tokio::runtime::Handle,
    ctx: egui::Context,
    inbox: Arc<Mutex<Vec<mdm_push::Push>>>,
    listener: Option<mdm_push::Listener>,
    /// The port it listens on now.
    port: u16,
    waiting: Vec<mdm_push::Push>,
    /// When each site last brought the window up (None: the pushes that name no site).
    raised: HashMap<Option<String>, Instant>,
    /// The site being asked about.
    pub ask: Option<SiteDialog>,
    /// Why it isn't listening, when `listen` failed (Options' status says it).
    pub error: Option<String>,
}

/// What the user said about a site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Allow,
    Block,
    NotNow,
}

/// What the Options → Push page says of the listener.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Off, as asked.
    Off,
    /// Listening on its port.
    Listening,
    /// Not listening: another program has the port.
    PortInUse,
    /// Not listening for another reason (the system's text).
    Error(String),
}

impl Inbox {
    pub fn new(rt: tokio::runtime::Handle, ctx: egui::Context) -> Self {
        Inbox {
            rt,
            ctx,
            inbox: Arc::default(),
            listener: None,
            port: mdm_push::PORT,
            waiting: Vec::new(),
            raised: HashMap::new(),
            ask: None,
            error: None,
        }
    }

    /// Listen (`on`) on `port`, or stop. Starting can fail, the port being taken: why, in the
    /// interface language. A port change stops the old listener and starts the new one.
    pub fn listen(&mut self, on: bool, port: u16) -> Result<(), String> {
        if !on {
            self.listener = None;
            self.port = port;
            self.waiting.clear();
            self.ask = None;
            self.error = None;
            return Ok(());
        }
        if self.listener.is_some() && self.port == port {
            return Ok(());
        }
        self.listener = None;
        let (inbox, ctx) = (self.inbox.clone(), self.ctx.clone());
        let on_push: mdm_push::OnPush = Arc::new(move |push| {
            let mut inbox = inbox.lock().unwrap();
            if inbox.len() >= WAITING {
                return false;
            }
            inbox.push(push);
            ctx.request_repaint();
            true
        });
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        match mdm_push::Listener::start(&self.rt, addr, on_push) {
            Ok(l) => {
                self.listener = Some(l);
                self.port = port;
                self.error = None;
                Ok(())
            }
            Err(e) => {
                tracing::warn!("Push can't listen on {addr}: {e}");
                let why = if e.kind() == std::io::ErrorKind::AddrInUse {
                    crate::i18n::fmt(
                        // i18n: notice: Push couldn't start; {port} is the port Options names
                        crate::i18n::t(
                            "Push can't start: another program is using port {port}. Close it, then turn Push off and on again in Options.",
                        ),
                        &[("port", &port.to_string())],
                    )
                } else {
                    crate::i18n::fmt(
                        // i18n: notice: Push couldn't start; {error} the system's reason
                        crate::i18n::t("Push can't start: {error}"),
                        &[("error", &e.to_string())],
                    )
                };
                self.error = Some(why.clone());
                Err(why)
            }
        }
    }

    /// What the listener is doing now, for Options' status.
    pub fn status(&self) -> Status {
        if self.listener.is_none() {
            match self.error.as_deref() {
                Some(e) if e.contains(&self.port.to_string()) => Status::PortInUse,
                Some(e) => Status::Error(e.to_owned()),
                _ => Status::Off,
            }
        } else {
            Status::Listening
        }
    }

    /// What arrived since the last frame, sorted by `sites` (Options' answers, site → allowed):
    /// an allowed site's pushes as downloads now; a blocked site's dropped; the others wait,
    /// and the first of them is asked about. With them, whether MDM's window comes up for it:
    /// once every [`RAISE_EVERY`] for each site.
    pub fn take(&mut self, sites: &[(String, bool)]) -> (Vec<ipc::Command>, bool) {
        self.take_at(sites, Instant::now())
    }

    fn take_at(&mut self, sites: &[(String, bool)], at: Instant) -> (Vec<ipc::Command>, bool) {
        let arrived = std::mem::take(&mut *self.inbox.lock().unwrap());
        let asking = self.ask.is_some();
        let mut raise = false;
        let mut now = Vec::new();
        for push in arrived {
            let said = push
                .site
                .as_ref()
                .and_then(|s| sites.iter().find(|(k, _)| k == s))
                .map(|(_, allowed)| *allowed);
            match said {
                Some(true) => {
                    raise |= self.may_raise(&push.site, at);
                    now.push(command(push));
                }
                Some(false) => {
                    tracing::info!("Push: a post from a blocked site, dropped");
                }
                None if self.waiting.len() < WAITING => self.waiting.push(push),
                None => tracing::info!("Push: too many posts waiting, one dropped"),
            }
        }
        self.ask_next();
        if !asking && let Some(site) = self.ask.as_ref().map(|a| a.site.clone()) {
            raise |= self.may_raise(&site, at);
        }
        (now, raise)
    }

    /// Whether `site` may bring the window up now: it hasn't within [`RAISE_EVERY`].
    fn may_raise(&mut self, site: &Option<String>, at: Instant) -> bool {
        self.raised
            .retain(|_, t| at.duration_since(*t) < RAISE_EVERY);
        if self.raised.contains_key(site) {
            return false;
        }
        self.raised.insert(site.clone(), at);
        true
    }

    /// The window's answer about the site asked: its waiting pushes (on Allow), and the answer
    /// to keep for the site (Allow, Block; none on Not now, or for a push that names no site).
    pub fn answer(&mut self, answer: Answer) -> (Vec<ipc::Command>, Option<(String, bool)>) {
        let Some(ask) = self.ask.take() else {
            return (Vec::new(), None);
        };
        let (theirs, others): (Vec<mdm_push::Push>, Vec<mdm_push::Push>) =
            std::mem::take(&mut self.waiting)
                .into_iter()
                .partition(|p| p.site == ask.site);
        self.waiting = others;
        let downloads = match answer {
            Answer::Allow => theirs.into_iter().map(command).collect(),
            Answer::Block | Answer::NotNow => Vec::new(),
        };
        let keep = match (answer, ask.site) {
            (Answer::NotNow, _) | (_, None) => None,
            (a, Some(site)) => Some((site, a == Answer::Allow)),
        };
        self.ask_next();
        (downloads, keep)
    }

    fn ask_next(&mut self) {
        if self.ask.is_some() {
            return;
        }
        if let Some(first) = self.waiting.first() {
            let site = first.site.clone();
            let count = self.waiting.iter().filter(|p| p.site == site).count();
            self.ask = Some(SiteDialog { site, count });
        }
    }
}

/// A push as a download: handed over like the browser's own, its site's cookies, Referer and
/// headers with it. The page it came from is kept whole (`page_url`), so Refresh download
/// address can open it again.
fn command(p: mdm_push::Push) -> ipc::Command {
    ipc::Command {
        url: Some(p.url),
        name: p.name,
        referer: p.referer.clone(),
        page_url: p.referer,
        origin: p.origin,
        cookies: p.cookies,
        user_agent: p.user_agent,
        extra_headers: p.extra_headers,
        size: p.size,
        silent: p.silent,
        later: p.later,
        ..Default::default()
    }
}

/// "Push from {site}": may this site push downloads?
pub struct SiteDialog {
    /// None: neither the browser nor the push names a site (an unknown site).
    pub site: Option<String>,
    /// The pushes it sent.
    count: usize,
}

impl SiteDialog {
    pub fn show(&mut self, ctx: &egui::Context) -> Outcome<Answer> {
        window(
            ctx,
            "push-site",
            // i18n: window title: a userscript pushed a download to this program
            "Push",
            [420.0, 116.0],
            false,
            LIGHT_BG(),
            |ui, o| {
                let p = ui.painter().clone();
                let n = self.count as u64;
                let line = match &self.site {
                    Some(site) => crate::i18n::fmt(
                        // i18n: Push window: {site} is a web site's name such as "example.com"
                        &crate::i18n::tn(
                            n,
                            "{site} wants to push {n} download to MDM.",
                            "{site} wants to push {n} downloads to MDM.",
                        ),
                        &[("site", &crate::i18n::isolate(site))],
                    ),
                    // i18n: Push window: a push whose web site neither the browser nor the push itself names
                    None => crate::i18n::tn(
                        n,
                        "An unknown site wants to push {n} download to MDM.",
                        "An unknown site wants to push {n} downloads to MDM.",
                    ),
                };
                winui::paragraph(
                    &p,
                    o.x + 11.0,
                    o.y + 21.0,
                    &line,
                    LIGHT_TEXT(),
                    o.x + 409.0,
                    14.0,
                );
                winui::paragraph(
                    &p,
                    o.x + 11.0,
                    o.y + 48.0,
                    // i18n: Push window, under the question: what a push carries, and that it may start at once
                    crate::i18n::t(
                        "Allow only sites you use. A push carries the site's cookies and headers, and may start its download at once.",
                    ),
                    winui::MUTED(),
                    o.x + 409.0,
                    14.0,
                );
                let row = winui::buttons_light_right(
                    ui,
                    o,
                    409.0,
                    84.0,
                    &[
                        // i18n: button in the Push window: let this site push downloads (remembered)
                        (crate::i18n::mark("Allow"), ButtonKind::Default),
                        // i18n: button in the Push window: refuse this site's pushes (remembered)
                        (crate::i18n::mark("Block"), ButtonKind::Normal),
                        // i18n: button in the Push window: refuse these, ask again next time
                        (crate::i18n::mark("Not now"), ButtonKind::Normal),
                    ],
                );
                if row[0].clicked() {
                    Outcome::Done(Answer::Allow)
                } else if row[1].clicked() {
                    Outcome::Done(Answer::Block)
                } else if row[2].clicked() || winui::escape_pressed(ui) {
                    Outcome::Done(Answer::NotNow)
                } else {
                    Outcome::Open
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdm_engine::Options;

    fn push(site: Option<&str>, url: &str) -> mdm_push::Push {
        mdm_push::Push {
            url: url.to_owned(),
            referer: Some("https://pan.example/s/list".to_owned()),
            cookies: Some("a=1".to_owned()),
            site: site.map(str::to_owned),
            ..Default::default()
        }
    }

    fn inbox() -> (tokio::runtime::Runtime, Inbox) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let i = Inbox::new(rt.handle().clone(), egui::Context::default());
        (rt, i)
    }

    #[test]
    fn a_push_is_a_download_with_its_context() {
        let c = command(mdm_push::Push {
            url: "https://dl.example/f.zip".to_owned(),
            name: Some("f.zip".to_owned()),
            referer: Some("https://pan.example/s".to_owned()),
            origin: Some("https://pan.example".to_owned()),
            cookies: Some("a=1".to_owned()),
            user_agent: Some("UA/1".to_owned()),
            extra_headers: vec![("Authorization".to_owned(), "Bearer t".to_owned())],
            silent: true,
            size: Some(9),
            site: Some("pan.example".to_owned()),
            ..Default::default()
        });
        assert_eq!(c.url.as_deref(), Some("https://dl.example/f.zip"));
        assert_eq!(c.name.as_deref(), Some("f.zip"));
        // The page is both the Referer and the page a refresh opens.
        assert_eq!(c.referer.as_deref(), c.page_url.as_deref());
        assert_eq!(c.origin.as_deref(), Some("https://pan.example"));
        assert_eq!(c.cookies.as_deref(), Some("a=1"));
        assert_eq!(c.user_agent.as_deref(), Some("UA/1"));
        assert_eq!(c.extra_headers.len(), 1);
        assert!(c.silent && !c.later);
        assert_eq!(c.size, Some(9));
    }

    #[test]
    fn a_site_is_asked_once_and_its_answer_kept() {
        let (_rt, mut i) = inbox();
        let mut sites = vec![
            ("ok.example".to_owned(), true),
            ("no.example".to_owned(), false),
        ];
        i.inbox.lock().unwrap().extend([
            push(Some("ok.example"), "https://dl.example/1.zip"),
            push(Some("no.example"), "https://dl.example/2.zip"),
            push(Some("new.example"), "https://dl.example/3.zip"),
            push(Some("new.example"), "https://dl.example/4.zip"),
        ]);
        // Allowed: now, as downloads with their context. Blocked: gone. New: asked once.
        let (now, raise) = i.take(&sites);
        assert!(raise);
        assert_eq!(now.len(), 1);
        assert_eq!(now[0].url.as_deref(), Some("https://dl.example/1.zip"));
        assert_eq!(now[0].cookies.as_deref(), Some("a=1"));
        let ask = i.ask.as_ref().unwrap();
        assert_eq!((ask.site.as_deref(), ask.count), (Some("new.example"), 2));
        let (downloads, keep) = i.answer(Answer::Allow);
        assert_eq!(downloads.len(), 2);
        assert_eq!(keep, Some(("new.example".to_owned(), true)));
        sites.push(keep.unwrap());
        assert!(i.ask.is_none());
        // Next time it isn't asked.
        i.inbox
            .lock()
            .unwrap()
            .push(push(Some("new.example"), "https://dl.example/5.zip"));
        assert_eq!(i.take(&sites).0.len(), 1);
        assert!(i.ask.is_none());
    }

    #[test]
    fn not_now_and_unnamed_sites_are_not_remembered() {
        let (_rt, mut i) = inbox();
        i.inbox.lock().unwrap().extend([
            push(Some("a.example"), "https://dl.example/1.zip"),
            push(None, "https://dl.example/2.zip"),
        ]);
        assert!(i.take(&[]).0.is_empty());
        let (downloads, keep) = i.answer(Answer::NotNow);
        assert!(downloads.is_empty() && keep.is_none());
        // The next one waiting is asked: an unknown site, never remembered.
        assert_eq!(i.ask.as_ref().unwrap().site, None);
        let (downloads, keep) = i.answer(Answer::Allow);
        assert_eq!((downloads.len(), keep), (1, None));
        assert!(i.ask.is_none());
    }

    #[test]
    fn a_site_brings_the_window_up_once_a_minute() {
        let (_rt, mut i) = inbox();
        let sites = vec![("ok.example".to_owned(), true)];
        let t = Instant::now();
        let send = |i: &mut Inbox, site: Option<&str>, at: Instant| {
            i.inbox
                .lock()
                .unwrap()
                .push(push(site, "https://dl.example/1.zip"));
            i.take_at(&sites, at)
        };
        let (downloads, raise) = send(&mut i, Some("ok.example"), t);
        assert_eq!((downloads.len(), raise), (1, true));
        for s in [1, 20, 59] {
            let (downloads, raise) = send(&mut i, Some("ok.example"), t + Duration::from_secs(s));
            assert_eq!((downloads.len(), raise), (1, false), "after {s} s");
        }
        let (_, raise) = send(&mut i, Some("ok.example"), t + Duration::from_secs(61));
        assert!(raise);
    }

    #[test]
    fn a_pushes_headers_reach_the_requests_and_leave_with_the_site() {
        let context = ipc::Command {
            extra_headers: vec![
                ("Authorization".to_owned(), "Bearer t".to_owned()),
                ("platform".to_owned(), "web".to_owned()),
                ("Cookie".to_owned(), "other=1".to_owned()),
                ("Range".to_owned(), "bytes=1-2".to_owned()),
                ("Bad Name".to_owned(), "x".to_owned()),
            ],
            ..Default::default()
        };
        let mut parts = RequestParts {
            headers: HeaderMap::new(),
            opts: Options::default(),
        };
        apply_extra_headers(&mut parts.headers, &context);
        // The site's own arrive; the engine's own names, and a name that isn't one, don't.
        assert_eq!(parts.headers.get("authorization").unwrap(), "Bearer t");
        assert_eq!(parts.headers.get("platform").unwrap(), "web");
        assert!(parts.headers.get("cookie").is_none());
        assert!(parts.headers.get("range").is_none());
        assert_eq!(parts.headers.len(), 2);
        // Out again, the way Download File Info takes them when its address changes site.
        remove_extra_headers(&mut parts, &context);
        assert!(parts.headers.is_empty());
    }
}
