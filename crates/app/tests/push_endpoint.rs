//! #1512: what a browser userscript pushes to the local listener arrives as a download, with
//! its site's cookies and headers. The real app, off the screens (MDM_OFFSCREEN), with a data
//! folder of its own: the listener on (port 0: the system's choice, read back from the log), a
//! post by hand over TCP. An allowed site's push opens Download File Info; a silent one is
//! added without it; the first push from an unknown site asks about it; a blocked site's is
//! dropped; a post that isn't ours (a form, another host's name, an address on this computer)
//! is refused, and nothing opens.
//!
//! It opens windows, which needs OpenGL or Direct3D, so it runs on request:
//! `cargo test -p mdm-app --test push_endpoint -- --ignored`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const APP: &str = env!("CARGO_BIN_EXE_misterdownloadmanager");

/// The app's log so far (every file of `logs/`).
fn log(data: &Path) -> String {
    let mut all = String::new();
    if let Ok(dir) = std::fs::read_dir(data.join("logs")) {
        for f in dir.flatten() {
            all += &std::fs::read_to_string(f.path()).unwrap_or_default();
        }
    }
    all
}

/// Wait until the log has `line`, failing if the app quits first or `wait` passes.
fn wait_for(app: &mut Child, data: &Path, line: &str, wait: Duration) {
    let until = Instant::now() + wait;
    loop {
        let text = log(data);
        assert!(!text.contains("panic"), "the app panicked:\n{text}");
        if text.contains(line) {
            return;
        }
        if let Some(status) = app.try_wait().unwrap() {
            panic!("the app closed ({status}) before '{line}':\n{text}");
        }
        assert!(
            Instant::now() < until,
            "no '{line}' after {wait:?}:\n{text}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The port the push listener took (`push_port: 0` in the settings: the system's choice), from
/// the log's own line about it.
fn listener_port(data: &Path) -> u16 {
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        for line in log(data).lines() {
            if let Some(at) = line.find("Push listening on 127.0.0.1:")
                && let Ok(p) = line[at + "Push listening on 127.0.0.1:".len()..]
                    .trim()
                    .parse()
            {
                return p;
            }
        }
        assert!(
            Instant::now() < until,
            "no listener line in the log:\n{}",
            log(data)
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// A `POST /push` by hand, answered with its status and body.
fn push(port: u16, origin: Option<&str>, content_type: Option<&str>, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let origin_line = origin
        .map(|o| format!("Origin: {o}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST /push HTTP/1.1\r\nHost: 127.0.0.1\r\n{origin_line}Content-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        content_type.unwrap_or("application/json"),
        body.len(),
    );
    s.write_all(request.as_bytes()).unwrap();
    let mut answer = String::new();
    s.read_to_string(&mut answer).unwrap();
    let status: u16 = answer[9..12].parse().unwrap();
    let body = answer
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    (status, body)
}

/// A push of `url` from `origin`, with the site's context.
fn push_body(url: &str, origin: &str, silent: bool) -> String {
    serde_json::json!({
        "url": url,
        "name": "file.zip",
        "referer": format!("{origin}/s/list"),
        "cookies": "a=1; b=2",
        "user_agent": "MDM-push-test/1.0",
        "extra_headers": [["Authorization", "Bearer test"]],
        "silent": silent,
    })
    .to_string()
}

/// A data folder of its own, with the listener on (port 0: the system's choice) and `sites`
/// the sites already answered about. The app itself the test spawns once over it.
fn start(push_sites: &serde_json::Value) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    // No update check, no first-start questions.
    let settings = serde_json::json!({
        "update_check": false, "helpers_asked": true, "language_asked": true,
        "extension_never_ask": true, "minimize_to_tray": false,
        "torrent_dht": false, "torrent_lsd": false, "torrent_upnp": false,
        "torrent_incoming": false,
        "push_on": true, "push_port": 0, "push_sites": push_sites,
    });
    std::fs::write(
        data.join("downloads.json"),
        serde_json::json!({ "next_id": 1, "items": [], "version": 2, "settings": settings })
            .to_string(),
    )
    .unwrap();
    dir
}

/// The app quits (its list saved), the test process waits for it.
fn stop(app: &mut Child, data: &Path) {
    let _ = Command::new(APP)
        .arg("/exit")
        .env("MDM_DATA_DIR", data)
        .env("MDM_OFFSCREEN", "1")
        .status();
    let until = Instant::now() + Duration::from_secs(15);
    while app.try_wait().unwrap().is_none() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = app.kill();
    let _ = app.wait();
}

/// The saved list's addresses.
fn saved_urls(data: &Path) -> Vec<String> {
    let list: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(data.join("downloads.json")).unwrap())
            .unwrap();
    list["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i["url"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
#[ignore = "opens the app's windows (off-screen): needs OpenGL or Direct3D; run with --ignored"]
fn an_allowed_sites_push_opens_the_file_info_dialog() {
    let dir = start(&serde_json::json!([["pan.example", true]]));
    let data = dir.path().to_path_buf();
    let mut app = Command::new(APP)
        .env("MDM_DATA_DIR", &data)
        .env("MDM_OFFSCREEN", "1")
        .spawn()
        .unwrap();
    let wait = Duration::from_secs(60);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wait_for(&mut app, &data, "window opened: main", wait);
        let port = listener_port(&data);
        let (status, body) = push(
            port,
            Some("https://pan.example"),
            None,
            &push_body(
                "http://push-test.example/file.zip",
                "https://pan.example",
                false,
            ),
        );
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"ok\":true"), "{body}");
        wait_for(&mut app, &data, "window opened: file-info", wait);
    }));
    stop(&mut app, &data);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

#[test]
#[ignore = "opens the app's windows (off-screen): needs OpenGL or Direct3D; run with --ignored"]
fn a_silent_push_is_added_without_a_dialog() {
    let dir = start(&serde_json::json!([["pan.example", true]]));
    let data = dir.path().to_path_buf();
    let mut app = Command::new(APP)
        .env("MDM_DATA_DIR", &data)
        .env("MDM_OFFSCREEN", "1")
        .spawn()
        .unwrap();
    let wait = Duration::from_secs(60);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wait_for(&mut app, &data, "window opened: main", wait);
        let port = listener_port(&data);
        let (status, body) = push(
            port,
            Some("https://pan.example"),
            None,
            &push_body(
                "http://push-test.example/silent.zip",
                "https://pan.example",
                true,
            ),
        );
        assert_eq!(status, 200, "{body}");
        // No dialog, no site question: the download is on the list (the app saves as it goes).
        std::thread::sleep(Duration::from_secs(2));
        assert!(
            !log(&data).contains("window opened: file-info")
                && !log(&data).contains("window opened: push-site"),
            "a silent push opened a window:\n{}",
            log(&data)
        );
        stop(&mut app, &data);
        let urls = saved_urls(&data);
        assert!(
            urls.iter()
                .any(|u| u == "http://push-test.example/silent.zip"),
            "the pushed download isn't in the saved list: {urls:?}"
        );
    }));
    if result.is_err() {
        let _ = Command::new(APP)
            .arg("/exit")
            .env("MDM_DATA_DIR", &data)
            .env("MDM_OFFSCREEN", "1")
            .status();
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

#[test]
#[ignore = "opens the app's windows (off-screen): needs OpenGL or Direct3D; run with --ignored"]
fn the_first_push_from_a_site_is_asked_about() {
    let dir = start(&serde_json::json!([]));
    let data = dir.path().to_path_buf();
    let mut app = Command::new(APP)
        .env("MDM_DATA_DIR", &data)
        .env("MDM_OFFSCREEN", "1")
        .spawn()
        .unwrap();
    let wait = Duration::from_secs(60);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wait_for(&mut app, &data, "window opened: main", wait);
        let port = listener_port(&data);
        let (status, body) = push(
            port,
            Some("https://new.example"),
            None,
            &push_body(
                "http://push-test.example/file.zip",
                "https://new.example",
                false,
            ),
        );
        assert_eq!(status, 200, "{body}");
        wait_for(&mut app, &data, "window opened: push-site", wait);
        // And the download itself waits for the answer: no file info yet.
        std::thread::sleep(Duration::from_secs(1));
        assert!(
            !log(&data).contains("window opened: file-info"),
            "the download didn't wait for the site's answer:\n{}",
            log(&data)
        );
    }));
    stop(&mut app, &data);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

#[test]
#[ignore = "opens the app's windows (off-screen): needs OpenGL or Direct3D; run with --ignored"]
fn a_blocked_sites_push_is_dropped_and_a_strangers_is_refused() {
    let dir = start(&serde_json::json!([["no.example", false]]));
    let data = dir.path().to_path_buf();
    let mut app = Command::new(APP)
        .env("MDM_DATA_DIR", &data)
        .env("MDM_OFFSCREEN", "1")
        .spawn()
        .unwrap();
    let wait = Duration::from_secs(60);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wait_for(&mut app, &data, "window opened: main", wait);
        let port = listener_port(&data);
        // The blocked site's push is taken (the listener can't say who is blocked), and the
        // app drops it: no question, no dialog.
        let (status, body) = push(
            port,
            Some("https://no.example"),
            None,
            &push_body(
                "http://push-test.example/file.zip",
                "https://no.example",
                false,
            ),
        );
        assert_eq!(status, 200, "{body}");
        // A form post (a page's no-cors fetch), a post naming another host, and an address on
        // this computer: refused.
        let (status, _) = push(
            port,
            None,
            Some("application/x-www-form-urlencoded"),
            "url=x",
        );
        assert_eq!(status, 415);
        let mut evil = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let request = format!(
            "POST /push HTTP/1.1\r\nHost: evil.example\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            push_body("http://push-test.example/f.zip", "https://x.example", false).len(),
            push_body("http://push-test.example/f.zip", "https://x.example", false),
        );
        evil.write_all(request.as_bytes()).unwrap();
        let mut answer = String::new();
        evil.read_to_string(&mut answer).unwrap();
        assert_eq!(&answer[9..12], "403");
        let (status, _) = push(
            port,
            Some("https://pan.example"),
            None,
            &serde_json::json!({"url": "http://127.0.0.1:9/admin"}).to_string(),
        );
        assert_eq!(status, 403);
        std::thread::sleep(Duration::from_secs(2));
        assert!(
            !log(&data).contains("window opened: file-info")
                && !log(&data).contains("window opened: push-site"),
            "a refused or dropped push opened a window:\n{}",
            log(&data)
        );
    }));
    stop(&mut app, &data);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
