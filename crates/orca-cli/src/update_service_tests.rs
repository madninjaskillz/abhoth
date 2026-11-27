use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A fake feed: answers `check` from a script, downloads in steps, never applies anything real.
struct Fake {
    answers: Mutex<Vec<Result<Option<String>, String>>>,
    download: Result<(), String>,
    gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    applied: AtomicUsize,
}

impl Fake {
    fn new(
        answers: Vec<Result<Option<String>, String>>,
        download: Result<(), String>,
    ) -> Arc<Fake> {
        Arc::new(Fake {
            answers: Mutex::new(answers),
            download,
            gate: Mutex::new(None),
            applied: AtomicUsize::new(0),
        })
    }
}

impl Feed for Fake {
    fn current(&self) -> String {
        "0.16.37".into()
    }
    fn channel(&self) -> String {
        "stable".into()
    }
    fn check(&self) -> Result<Option<String>, String> {
        let gate = self.gate.lock().unwrap().take();
        if let Some(g) = gate {
            let _ = g.recv_timeout(Duration::from_secs(5));
        }
        self.answers.lock().unwrap().remove(0)
    }
    fn download(&self, progress: Arc<dyn Fn(u8) + Send + Sync>) -> Result<(), String> {
        for p in [10, 55, 100] {
            progress(p);
        }
        self.download.clone()
    }
    fn apply(&self) -> Result<(), String> {
        self.applied.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn service(f: &Arc<Fake>) -> (Arc<Updates>, Arc<Mutex<Vec<String>>>) {
    let ready = Arc::new(Mutex::new(Vec::new()));
    let r = ready.clone();
    let feed: Arc<dyn Feed> = f.clone();
    let u = Updates::new(Some(feed), move |v| r.lock().unwrap().push(v));
    (u, ready)
}

#[test]
fn up_to_date_then_a_new_release_is_downloaded_and_offered() {
    let f = Fake::new(vec![Ok(None), Ok(Some("0.16.38".into()))], Ok(()));
    let (u, ready) = service(&f);
    let s = u.status();
    assert_eq!(
        (s.phase.as_str(), s.current.as_str(), s.channel.as_str()),
        ("idle", "0.16.37", "stable")
    );
    assert!(s.last_check.is_none());

    assert!(u.check("you"));
    let s = u.status();
    assert_eq!(s.phase, "upToDate");
    assert_eq!(s.last_result.as_deref(), Some("Up to date (0.16.37)."));
    assert!(s.last_check.is_some());
    assert_eq!(s.by.as_deref(), Some("you"));

    assert!(u.check("automatic"));
    let s = u.status();
    assert_eq!(s.phase, "ready");
    assert_eq!(s.available.as_deref(), Some("0.16.38"));
    assert_eq!(s.progress, Some(100));
    assert_eq!(
        s.notes_url.as_deref(),
        Some("https://github.com/madninjaskillz/abhoth/releases/tag/abhoth-v0.16.38")
    );
    assert_eq!(*ready.lock().unwrap(), vec!["0.16.38".to_string()]);
    // nothing more to check while one is ready
    assert!(!u.check("you"));
}

#[test]
fn errors_are_said_plainly_and_a_later_check_can_still_succeed() {
    let f = Fake::new(
        vec![
            Err("error sending request: dns error: failed to lookup address".into()),
            Err("HTTP status client error (404 Not Found)".into()),
            Ok(None),
        ],
        Ok(()),
    );
    let (u, _) = service(&f);
    u.check("you");
    let s = u.status();
    assert_eq!(s.phase, "error");
    assert!(
        s.error
            .as_deref()
            .unwrap()
            .starts_with("Couldn't reach GitHub: this PC looks offline"),
        "{s:?}"
    );
    assert!(
        s.last_result
            .as_deref()
            .unwrap()
            .starts_with("Check failed")
    );
    u.check("you");
    assert!(
        u.status()
            .error
            .as_deref()
            .unwrap()
            .starts_with("The releases on GitHub couldn't be read")
    );
    u.check("you");
    let s = u.status();
    assert_eq!(s.phase, "upToDate");
    assert!(s.error.is_none());
}

#[test]
fn a_failed_download_keeps_the_version_found_and_says_so() {
    let f = Fake::new(
        vec![Ok(Some("0.16.38".into()))],
        Err("connection reset; timed out".into()),
    );
    let (u, ready) = service(&f);
    u.check("you");
    let s = u.status();
    assert_eq!(s.phase, "error");
    assert_eq!(s.available.as_deref(), Some("0.16.38"));
    assert_eq!(
        s.last_result.as_deref(),
        Some("0.16.38 was found, but the download failed.")
    );
    assert!(ready.lock().unwrap().is_empty());
    assert!(u.apply().is_err());
}

#[test]
fn check_now_answers_checking_at_once_and_a_second_press_does_not_start_another() {
    let f = Fake::new(vec![Ok(None)], Ok(()));
    let (tx, rx) = std::sync::mpsc::channel();
    *f.gate.lock().unwrap() = Some(rx);
    let (u, _) = service(&f);
    let s = u.check_now().unwrap();
    assert_eq!(
        (s.phase.as_str(), s.by.as_deref()),
        ("checking", Some("you"))
    );
    // pressed again while checking: same state, no second check (the script has one answer)
    assert_eq!(u.check_now().unwrap().phase, "checking");
    tx.send(()).unwrap();
    for _ in 0..200 {
        if u.status().phase != "checking" {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(u.status().phase, "upToDate");
}

#[test]
fn apply_goes_through_the_apps_hook_only_when_ready() {
    let f = Fake::new(vec![Ok(Some("0.16.38".into()))], Ok(()));
    let (u, _) = service(&f);
    assert!(u.apply().unwrap_err().contains("check for updates first"));
    u.check("you");
    assert!(u.apply().unwrap_err().contains("can't restart"));
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    u.set_apply(move || {
        h.fetch_add(1, Ordering::SeqCst);
    });
    u.apply().unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    // the hook decides when the feed applies (after the app stopped its host)
    assert_eq!(f.applied.load(Ordering::SeqCst), 0);
    u.apply_feed().unwrap();
    assert_eq!(f.applied.load(Ordering::SeqCst), 1);
}

#[test]
fn a_dev_build_has_no_feed_and_says_why() {
    let u = Updates::new(None, |_| {});
    let s = u.status();
    assert_eq!(s.phase, "unavailable");
    assert!(s.reason.as_deref().unwrap().contains("dev build"));
    assert!(!u.check("you"));
    assert!(u.check_now().unwrap_err().contains("installer"));
}

#[tokio::test]
async fn the_http_calls_answer_status_check_and_apply() {
    let f = Fake::new(vec![Ok(Some("0.16.38".into()))], Ok(()));
    let (u, _) = service(&f);
    let call = |m: Method, p: &str| {
        let u = u.clone();
        let req = Request::builder()
            .method(m)
            .uri(p)
            .body(Body::empty())
            .unwrap();
        async move {
            let r = handle(u, req).await;
            let s = r.status().as_u16();
            let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
            (s, serde_json::from_slice::<Value>(&b).unwrap())
        }
    };
    let (s, v) = call(Method::GET, "/api/desktop/updates").await;
    assert_eq!((s, v["phase"].as_str()), (200, Some("idle")));
    let (s, v) = call(Method::POST, "/api/desktop/updates/apply").await;
    assert_eq!(s, 409, "{v}");
    let (s, v) = call(Method::POST, "/api/desktop/updates/check").await;
    assert_eq!((s, v["phase"].as_str()), (202, Some("checking")));
    for _ in 0..200 {
        if u.status().phase == "ready" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (_, v) = call(Method::GET, "/api/desktop/updates").await;
    assert_eq!(v["phase"], "ready");
    assert_eq!(v["available"], "0.16.38");
    assert_eq!(
        v["notesUrl"],
        "https://github.com/madninjaskillz/abhoth/releases/tag/abhoth-v0.16.38"
    );
    u.set_apply(|| {});
    let (s, _) = call(Method::POST, "/api/desktop/updates/apply").await;
    assert_eq!(s, 202);
    assert_eq!(
        f.applied.load(Ordering::SeqCst),
        0,
        "the page never applies by itself"
    );
    let (s, _) = call(Method::DELETE, "/api/desktop/updates").await;
    assert_eq!(s, 404);
}

#[test]
fn friendly_errors() {
    assert!(friendly("operation timed out").starts_with("Couldn't reach GitHub"));
    assert!(friendly("API rate limit exceeded (403)").starts_with("The releases on GitHub"));
    assert!(friendly("sha256 checksum mismatch").starts_with("The download was damaged"));
    assert_eq!(friendly("something else"), "something else");
}
