//! Account-scoped YouTube playback metadata and watch reporting. Signed URLs,
//! credentials and responses stay in memory and never enter logs or disk caches.
use anyhow::{Result, anyhow, ensure};
use reqwest::{
    Url,
    header::{AUTHORIZATION, COOKIE, HeaderMap, HeaderValue},
};
use rustypipe::client::{ClientType, RustyPipe};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    runtime::Handle,
    sync::{Notify, mpsc},
    task::JoinHandle,
};

/// How long a resume lookup waits for the same video's final report to land.
const REPORT_WAIT: Duration = Duration::from_secs(8);
const METADATA_TIMEOUT: Duration = Duration::from_secs(10);
/// Failed reports are resent on this interval while the viewing stays open.
const RETRY_INTERVAL: Duration = Duration::from_secs(15);
use zeroize::Zeroizing;

pub struct Client {
    pub(super) pipe: RustyPipe,
    http: reqwest::Client,
    cookie: Zeroizing<String>,
    headers: HeaderMap,
    reports: Arc<tokio::sync::Mutex<()>>,
    /// Videos whose reporter is still sending, with a count per open viewing.
    pending: Mutex<HashMap<String, usize>>,
    settled: Notify,
}

#[derive(Clone)]
pub struct Info {
    id: String,
    pub resume: Option<f64>,
    pub duration: f64,
    playback: Option<Url>,
    watchtime: Option<Url>,
}

impl Client {
    pub fn new(
        pipe: RustyPipe,
        cookies: &str,
        identity: &crate::history::IdentityCapture,
    ) -> Result<Arc<Self>> {
        let (index, channel) = identity
            .playback_headers()
            .ok_or_else(|| anyhow!("Missing playback identity"))?;
        let mut headers = HeaderMap::new();
        headers.insert("x-goog-authuser", HeaderValue::from_str(&index)?);
        if let Some(channel) = channel {
            headers.insert("x-goog-pageid", HeaderValue::from_str(&channel)?);
        }
        headers.insert(
            "origin",
            HeaderValue::from_static("https://www.youtube.com"),
        );
        headers.insert(
            "referer",
            HeaderValue::from_static("https://www.youtube.com/"),
        );
        Ok(Arc::new(Self {
            pipe,
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(5))
                .build()?,
            cookie: super::resolve::cookie_header(cookies)?,
            headers,
            reports: report_order(
                identity
                    .account()
                    .ok_or_else(|| anyhow!("Missing account identity"))?
                    .filename(),
            ),
            pending: Mutex::default(),
            settled: Notify::new(),
        }))
    }

    /// `resume` waits (bounded) for this video's earlier viewing to finish
    /// reporting, so YouTube returns the position from its final report.
    /// Other videos and stream refreshes never wait on reports.
    pub async fn info(&self, id: &str, resume: bool) -> Result<Info> {
        ensure!(super::resolve::valid_id(id), "Invalid video");
        if resume {
            let _ = tokio::time::timeout(REPORT_WAIT, self.settled(id)).await;
        }
        let raw = tokio::time::timeout(
            METADATA_TIMEOUT,
            self.pipe.query().authenticated().raw(
                ClientType::Desktop,
                "player",
                &json!({
                    "videoId": id, "contentCheckOk": true, "racyCheckOk": true
                }),
            ),
        )
        .await
        .map_err(|_| anyhow!("Timed out fetching playback metadata"))?
        .map_err(|_| anyhow!("Could not fetch playback metadata"))?;
        let value: Value =
            serde_json::from_str(&raw).map_err(|_| anyhow!("Invalid playback metadata"))?;
        ensure!(
            value["videoDetails"]["videoId"].as_str() == Some(id),
            "Unexpected playback metadata"
        );
        Ok(Info::parse(&value))
    }

    async fn settled(&self, id: &str) {
        loop {
            let notified = self.settled.notified();
            tokio::pin!(notified);
            // Register before checking so a report finishing in between still wakes us.
            notified.as_mut().enable();
            if !self.pending.lock().unwrap().contains_key(id) {
                return;
            }
            notified.await;
        }
    }

    async fn ping(&self, url: Url) -> Result<()> {
        #[cfg(test)]
        let diagnostic = if std::env::var_os("SLINTYT_TEST_PROGRESS").is_some() {
            Some((
                url.path().to_owned(),
                url.query_pairs()
                    .find(|(k, _)| k == "cmt")
                    .map(|(_, v)| v.into_owned()),
            ))
        } else {
            None
        };
        let mut headers = self.headers.clone();
        let mut cookie = HeaderValue::from_str(&self.cookie)?;
        cookie.set_sensitive(true);
        headers.insert(COOKIE, cookie);
        if let Some(sapisid) = self
            .cookie
            .split(';')
            .find_map(|c| c.trim().strip_prefix("SAPISID="))
        {
            let now = crate::history::now();
            let mut digest = Sha1::new();
            digest.update(format!("{now} "));
            digest.update(sapisid);
            digest.update(" https://www.youtube.com");
            let mut auth =
                HeaderValue::from_str(&format!("SAPISIDHASH {now}_{:x}", digest.finalize()))?;
            auth.set_sensitive(true);
            headers.insert(AUTHORIZATION, auth);
        }
        let result = self.http.get(url).headers(headers).send().await;
        #[cfg(test)]
        if let Some((path, position)) = diagnostic {
            println!(
                "Progress report {path} position={position:?} HTTP={:?}",
                result.as_ref().ok().map(|r| r.status().as_u16())
            );
        }
        ensure!(
            result.is_ok_and(|r| r.status().is_success()),
            "YouTube progress update failed"
        );
        Ok(())
    }
}

/// Marks a video as still reporting until its reporter task ends (or is aborted).
struct Pending {
    client: Arc<Client>,
    video: String,
}

impl Pending {
    fn new(client: Arc<Client>, video: &str) -> Self {
        *client
            .pending
            .lock()
            .unwrap()
            .entry(video.to_owned())
            .or_default() += 1;
        Self {
            client,
            video: video.to_owned(),
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        let mut pending = self.client.pending.lock().unwrap();
        if let Some(count) = pending.get_mut(&self.video) {
            *count -= 1;
            if *count == 0 {
                pending.remove(&self.video);
            }
        }
        drop(pending);
        self.client.settled.notify_waiters();
    }
}

fn report_order(account: String) -> Arc<tokio::sync::Mutex<()>> {
    use std::{
        collections::HashMap,
        sync::{OnceLock, Weak},
    };
    type Orders = Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>;
    static ORDERS: OnceLock<Orders> = OnceLock::new();
    let mut orders = ORDERS.get_or_init(Mutex::default).lock().unwrap();
    orders.retain(|_, order| order.strong_count() > 0);
    if let Some(order) = orders.get(&account).and_then(Weak::upgrade) {
        return order;
    }
    let order = Arc::new(tokio::sync::Mutex::new(()));
    orders.insert(account, Arc::downgrade(&order));
    order
}

fn seconds(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|n| n.is_finite() && *n >= 0.)
}

impl Info {
    fn parse(value: &Value) -> Self {
        let duration = seconds(&value["videoDetails"]["lengthSeconds"]).unwrap_or(0.);
        let resume = seconds(&value["playerConfig"]["playbackStartConfig"]["startSeconds"])
            .filter(|p| *p > 0. && (duration == 0. || *p < duration - 2.))
            .filter(|_| value["videoDetails"]["isLive"] != true);
        Self {
            id: value["videoDetails"]["videoId"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            resume,
            duration,
            playback: tracking_url(
                &value["playbackTracking"]["videostatsPlaybackUrl"]["baseUrl"],
                "playback",
            ),
            watchtime: tracking_url(
                &value["playbackTracking"]["videostatsWatchtimeUrl"]["baseUrl"],
                "watchtime",
            ),
        }
    }
}

fn tracking_url(value: &Value, kind: &str) -> Option<Url> {
    let mut url = Url::parse(value.as_str()?).ok()?;
    if url.scheme() != "https"
        || !matches!(url.host_str(), Some("www.youtube.com" | "s.youtube.com"))
        || url.path() != format!("/api/stats/{kind}")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|port| port != 443)
    {
        return None;
    }
    // Like YouTube.js, use the web origin for authenticated stats requests.
    url.set_host(Some("www.youtube.com")).ok()?;
    Some(url)
}

pub type Tasks = Arc<Mutex<Vec<JoinHandle<()>>>>;
pub type Status = Arc<dyn Fn(bool) + Send + Sync>;

#[derive(Clone, Copy)]
pub struct Sample {
    pub position: f64,
    pub duration: f64,
    pub paused: bool,
    pub buffering: bool,
    pub speed: f64,
}
impl Sample {
    fn playing(self) -> bool {
        !self.paused && !self.buffering
    }
}

struct Report {
    position: f64,
    duration: f64,
    elapsed: f64,
    playing: bool,
    final_report: bool,
    segments: Vec<(f64, f64)>,
}

/// Everything needed to start a reporting session, so a replay can begin a new one.
struct Reporter {
    client: Arc<Client>,
    playback: Url,
    watchtime: Url,
    video: String,
    runtime: Handle,
    tasks: Tasks,
    status: Status,
    retry: Duration,
}

impl Reporter {
    /// One YouTube viewing: its own playback nonce, announced once, then watchtime reports.
    fn spawn(&self) -> mpsc::Sender<Report> {
        let (sender, mut receiver) = mpsc::channel::<Report>(32);
        let pending = Pending::new(self.client.clone(), &self.video);
        let (client, playback, watchtime, status, retry) = (
            self.client.clone(),
            self.playback.clone(),
            self.watchtime.clone(),
            self.status.clone(),
            self.retry,
        );
        let cpn = nonce();
        let task = self.runtime.spawn(async move {
            let _pending = pending;
            // Prevent a delayed final report from overwriting a newer viewing.
            let _order = client.reports.lock().await;
            let mut announced = false;
            let mut missed = Vec::new();
            let mut failed: Option<Report> = None;
            loop {
                let mut report = match failed.take() {
                    // Keep retrying while the viewing is open, even when paused.
                    // Once closed, the final report has had its attempts.
                    Some(last) => match tokio::time::timeout(retry, receiver.recv()).await {
                        Ok(Some(next)) => next,
                        Ok(None) => break,
                        Err(_) => last,
                    },
                    None => match receiver.recv().await {
                        Some(report) => report,
                        None => break,
                    },
                };
                while let Ok(mut next) = receiver.try_recv() {
                    next.segments.splice(0..0, report.segments);
                    report = next;
                }
                missed.append(&mut report.segments);
                // Bound retries/segments during a long outage without ever claiming seek gaps.
                if missed.len() > 128 {
                    missed.drain(..missed.len() - 128);
                }
                report.segments = missed.clone();
                let mut succeeded = false;
                for attempt in 0..2 {
                    if attempt > 0 {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                    if !announced {
                        announced = client
                            .ping(report.url(&playback, &cpn, false))
                            .await
                            .is_ok();
                    }
                    if announced
                        && client
                            .ping(report.url(&watchtime, &cpn, true))
                            .await
                            .is_ok()
                    {
                        succeeded = true;
                        break;
                    }
                }
                if succeeded {
                    missed.clear();
                } else {
                    // Unsent segments stay in `missed` and go out with the retry.
                    failed = Some(Report {
                        segments: Vec::new(),
                        ..report
                    });
                }
                status(succeeded);
            }
        });
        let mut jobs = self.tasks.lock().unwrap();
        jobs.retain(|job| !job.is_finished());
        jobs.push(task);
        sender
    }
}

pub struct Tracker {
    reporter: Option<Reporter>,
    sender: Option<mpsc::Sender<Report>>,
    runtime: Handle,
    previous: Option<(Sample, Instant)>,
    segments: Vec<(f64, f64)>,
    began: Instant,
    last_report: Instant,
    started: bool,
    force: bool,
    discontinuous: bool,
    finished: bool,
    duration: f64,
}

impl Tracker {
    pub fn new(
        client: Arc<Client>,
        info: Info,
        runtime: Handle,
        tasks: &Tasks,
        status: impl Fn(bool) + Send + Sync + 'static,
    ) -> Option<Self> {
        let reporter = Reporter {
            client,
            playback: info.playback?,
            watchtime: info.watchtime?,
            video: info.id,
            runtime: runtime.clone(),
            tasks: tasks.clone(),
            status: Arc::new(status),
            retry: RETRY_INTERVAL,
        };
        let sender = reporter.spawn();
        let mut tracker = Self::idle(runtime, info.duration);
        tracker.reporter = Some(reporter);
        tracker.sender = Some(sender);
        Some(tracker)
    }

    /// Collect real watch samples while the non-blocking metadata lookup runs.
    pub fn pending(runtime: Handle) -> Self {
        Self::idle(runtime, 0.)
    }

    pub fn inherit_samples(&mut self, mut pending: Self) {
        self.previous = pending.previous.take();
        self.segments = std::mem::take(&mut pending.segments);
        self.began = pending.began;
        self.last_report = pending.last_report;
        self.started = pending.started;
        self.force = pending.force;
        self.discontinuous = pending.discontinuous;
        if pending.duration > 0. {
            self.duration = pending.duration;
        }
        self.flush(pending.finished);
    }

    fn idle(runtime: Handle, duration: f64) -> Self {
        let now = Instant::now();
        Self {
            reporter: None,
            sender: None,
            runtime,
            previous: None,
            segments: Vec::new(),
            began: now,
            last_report: now,
            started: false,
            force: false,
            discontinuous: false,
            finished: false,
            duration,
        }
    }

    pub fn observe(&mut self, mut sample: Sample, now: Instant) {
        if self.finished {
            return;
        }
        if !sample.position.is_finite()
            || sample.position < 0.
            || !sample.duration.is_finite()
            || !sample.speed.is_finite()
        {
            return;
        }
        if sample.duration <= 0. {
            sample.duration = self.duration;
        } else {
            self.duration = sample.duration;
        }
        // Seeking is asynchronous. Keep excluding movement until mpv confirms
        // PlaybackRestart, even if intermediate samples still show the old frame.
        if self.discontinuous {
            self.previous = Some((sample, now));
            return;
        }
        let mut changed = false;
        if let Some((old, at)) = self.previous {
            let delta = sample.position - old.position;
            let continuous = delta >= 0.
                && delta <= now.duration_since(at).as_secs_f64() * old.speed.max(1.) * 1.5 + 1.;
            if continuous && old.playing() && delta > 0. {
                self.started = true;
                if let Some(last) = self
                    .segments
                    .last_mut()
                    .filter(|last| (last.1 - old.position).abs() < 0.01)
                {
                    last.1 = sample.position;
                } else {
                    if self.segments.len() >= 128 {
                        self.segments.remove(0);
                    }
                    self.segments.push((old.position, sample.position));
                }
            }
            changed = old.paused != sample.paused
                || old.buffering != sample.buffering
                || old.speed != sample.speed
                || !continuous;
        }
        self.previous = Some((sample, now));
        if self.started
            && (changed
                || self.force
                || self.last_report == self.began
                || (sample.playing()
                    && now.duration_since(self.last_report) >= Duration::from_secs(20)))
        {
            self.flush(false);
        }
    }

    /// `user` is an explicit seek or replay. It counts as engagement, so the new
    /// position is reported even if playback never advanced. mpv's own seek event
    /// for the same jump is then a no-op instead of a second report.
    pub fn seek(&mut self, user: bool) {
        if self.finished {
            if !user {
                return;
            }
            self.restart_session();
        } else if !self.discontinuous {
            self.flush(false);
        }
        self.discontinuous = true;
        self.force = true;
        self.started |= user;
    }

    /// A replay after the final report is a new YouTube viewing with its own nonce.
    fn restart_session(&mut self) {
        if let Some(reporter) = &self.reporter {
            self.sender = Some(reporter.spawn());
        }
        let now = Instant::now();
        self.previous = None;
        self.segments.clear();
        self.began = now;
        self.last_report = now;
        self.started = false;
        self.finished = false;
    }

    pub fn restarted(&mut self) {
        self.previous = None;
        self.discontinuous = false;
        self.force = true;
    }

    pub fn flush(&mut self, final_report: bool) {
        if !self.started || self.finished {
            return;
        }
        self.finished = final_report;
        let Some((sample, _)) = self.previous else {
            return;
        };
        // Keep samples until metadata supplies a reporter. Even a short video
        // that ends before that lookup completes retains its final report.
        let Some(sender) = &self.sender else {
            return;
        };
        let report = Report {
            position: sample.position,
            duration: sample.duration,
            elapsed: self.began.elapsed().as_secs_f64(),
            playing: sample.playing() && !final_report,
            final_report,
            segments: std::mem::take(&mut self.segments),
        };
        match sender.try_send(report) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(report)) if final_report => {
                // Preserve the final position even if earlier reports are still queued.
                let sender = sender.clone();
                self.runtime.spawn(async move {
                    let _ = sender.send(report).await;
                });
            }
            Err(mpsc::error::TrySendError::Full(report)) => {
                self.segments = report.segments;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
        self.force = false;
        self.last_report = Instant::now();
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        self.flush(true);
    }
}

fn nonce() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    use sha2::Sha256;
    let seed = format!(
        "{:?}-{}-{}",
        std::time::SystemTime::now(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    format!("{:x}", Sha256::digest(seed.as_bytes()))[..16].to_owned()
}

impl Report {
    fn url(&self, base: &Url, cpn: &str, watchtime: bool) -> Url {
        let mut url = base.clone();
        let replaced = [
            "ver", "cpn", "cmt", "st", "et", "rt", "rtn", "len", "state", "final",
        ];
        let pairs = url
            .query_pairs()
            .filter(|(k, _)| !replaced.contains(&k.as_ref()))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect::<Vec<_>>();
        url.set_query(None);
        let mut query = url.query_pairs_mut();
        query.extend_pairs(pairs);
        query
            .append_pair("ver", "2")
            .append_pair("cpn", cpn)
            .append_pair("cmt", &format!("{:.3}", self.position))
            .append_pair("rt", &format!("{:.3}", self.elapsed))
            .append_pair("rtn", &format!("{:.3}", self.elapsed + 20.))
            .append_pair("len", &format!("{:.3}", self.duration))
            .append_pair("state", if self.playing { "playing" } else { "paused" });
        if watchtime {
            let segments = if self.segments.is_empty() {
                vec![(self.position, self.position)]
            } else {
                self.segments.clone()
            };
            query
                .append_pair(
                    "st",
                    &segments
                        .iter()
                        .map(|s| format!("{:.3}", s.0))
                        .collect::<Vec<_>>()
                        .join(","),
                )
                .append_pair(
                    "et",
                    &segments
                        .iter()
                        .map(|s| format!("{:.3}", s.1))
                        .collect::<Vec<_>>()
                        .join(","),
                )
                .append_pair("final", if self.final_report { "1" } else { "0" });
        }
        drop(query);
        url
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(position: f64, paused: bool) -> Sample {
        Sample {
            position,
            paused,
            duration: 600.,
            buffering: false,
            speed: 1.,
        }
    }

    fn test_tracker() -> (Tracker, mpsc::Receiver<Report>) {
        let (sender, receiver) = mpsc::channel(32);
        let mut tracker = Tracker::idle(Handle::current(), 600.);
        tracker.sender = Some(sender);
        (tracker, receiver)
    }

    #[tokio::test]
    async fn late_metadata_retains_watched_segments_and_the_final_position() {
        let now = Instant::now();
        let mut pending = Tracker::pending(Handle::current());
        pending.observe(sample(0., false), now);
        pending.observe(sample(2., false), now + Duration::from_secs(2));
        pending.seek(true);
        pending.restarted();
        pending.observe(sample(20., false), now + Duration::from_secs(3));
        pending.observe(sample(22., false), now + Duration::from_secs(5));
        pending.flush(true);
        let (mut tracker, mut reports) = test_tracker();
        tracker.inherit_samples(pending);
        let report = reports.recv().await.unwrap();
        assert_eq!(report.segments, [(0., 2.), (20., 22.)]);
        assert_eq!(report.position, 22.);
        assert!(report.final_report);
        assert!(reports.try_recv().is_err());
    }

    fn test_client() -> Arc<Client> {
        Arc::new(Client {
            pipe: RustyPipe::builder()
                .storage(Box::new(crate::history::IdentityCapture::default()))
                .no_reporter()
                .build()
                .unwrap(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            cookie: Zeroizing::new("SAPISID=test".into()),
            headers: HeaderMap::new(),
            reports: Arc::new(tokio::sync::Mutex::new(())),
            pending: Mutex::default(),
            settled: Notify::new(),
        })
    }

    /// Records each stats request path and query; fails the first `failures` watchtime pings.
    async fn stats_server(failures: usize) -> (String, Arc<Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            let mut failures = failures;
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let Ok(read) = socket.read(&mut buffer).await else {
                        break;
                    };
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                let line = String::from_utf8_lossy(&request)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .split(' ')
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                let fail = line.starts_with("/api/stats/watchtime") && failures > 0;
                if fail {
                    failures -= 1;
                }
                log.lock().unwrap().push(line);
                let status = if fail {
                    "503 Service Unavailable"
                } else {
                    "200 OK"
                };
                let _ = socket
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        (base, seen)
    }

    fn test_reporter(client: Arc<Client>, base: &str, status: Status) -> Reporter {
        Reporter {
            client,
            playback: Url::parse(&format!("{base}/api/stats/playback?docid=video")).unwrap(),
            watchtime: Url::parse(&format!("{base}/api/stats/watchtime?docid=video")).unwrap(),
            video: "aqz-KE-bpKQ".into(),
            runtime: Handle::current(),
            tasks: Tasks::default(),
            status,
            retry: Duration::from_millis(200),
        }
    }

    fn query(line: &str, key: &str) -> Option<String> {
        Url::parse(&format!("http://localhost{line}"))
            .ok()?
            .query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    }

    async fn wait_until(mut done: impl FnMut() -> bool) {
        for _ in 0..200 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("Timed out waiting for reports");
    }

    #[test]
    fn user_seeks_count_as_engagement_but_mpv_seeks_do_not() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        // An mpv-initiated seek (e.g. the resume offset) is not a viewing.
        let (mut tracker, mut receiver) = test_tracker();
        tracker.seek(false);
        tracker.restarted();
        tracker.observe(sample(120., true), Instant::now());
        drop(tracker);
        assert!(receiver.try_recv().is_err());

        // Scrubbing without playing reports the new position, then a final one on close.
        let (mut tracker, mut receiver) = test_tracker();
        tracker.observe(sample(0., true), Instant::now());
        tracker.seek(true);
        assert!(receiver.try_recv().is_err()); // nothing watched before the seek
        tracker.seek(false);
        tracker.restarted();
        tracker.observe(sample(300., true), Instant::now());
        let report = receiver.try_recv().unwrap();
        assert_eq!(report.position, 300.);
        assert!(report.segments.is_empty() && !report.playing);
        drop(tracker);
        let last = receiver.try_recv().unwrap();
        assert!(last.final_report);
        assert_eq!(last.position, 300.);
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn replay_after_the_final_report_starts_a_new_youtube_viewing() {
        let (base, seen) = stats_server(0).await;
        let client = test_client();
        let mut tracker = Tracker::idle(Handle::current(), 600.);
        let reporter = test_reporter(client.clone(), &base, Arc::new(|_| {}));
        tracker.sender = Some(reporter.spawn());
        tracker.reporter = Some(reporter);
        let now = Instant::now();
        tracker.observe(sample(590., false), now);
        tracker.observe(sample(600., false), now + Duration::from_secs(10));
        tracker.flush(true); // end of file
        tracker.seek(false); // mpv seeking after EOF is not a replay
        assert!(tracker.finished);
        tracker.seek(true); // replay
        assert!(!tracker.finished);
        tracker.restarted();
        tracker.observe(sample(0., false), now + Duration::from_secs(11));
        tracker.observe(sample(1., false), now + Duration::from_secs(12));
        drop(tracker);
        wait_until(|| client.pending.lock().unwrap().is_empty()).await;
        let seen = seen.lock().unwrap().clone();
        let playback = seen
            .iter()
            .filter(|l| l.starts_with("/api/stats/playback"))
            .collect::<Vec<_>>();
        assert_eq!(playback.len(), 2, "each viewing is announced once");
        let first = query(playback[0], "cpn");
        let second = query(playback[1], "cpn");
        assert!(first.is_some() && first != second);
        let finals = seen
            .iter()
            .filter(|l| {
                l.starts_with("/api/stats/watchtime") && query(l, "final").as_deref() == Some("1")
            })
            .map(|l| query(l, "cpn"))
            .collect::<Vec<_>>();
        assert_eq!(finals, [first, second]);
    }

    #[tokio::test]
    async fn failed_reports_are_retried_while_paused_and_keep_their_segments() {
        let (base, seen) = stats_server(2).await;
        let client = test_client();
        let statuses = Arc::new(Mutex::new(Vec::new()));
        let log = statuses.clone();
        let reporter = test_reporter(
            client.clone(),
            &base,
            Arc::new(move |ok| log.lock().unwrap().push(ok)),
        );
        let mut tracker = Tracker::idle(Handle::current(), 600.);
        tracker.sender = Some(reporter.spawn());
        tracker.reporter = Some(reporter);
        let now = Instant::now();
        tracker.observe(sample(10., false), now);
        tracker.observe(sample(15., false), now + Duration::from_secs(5));
        tracker.observe(sample(15., true), now + Duration::from_secs(6));
        // Paused: no new reports arrive, yet the failed one is resent.
        wait_until(|| statuses.lock().unwrap().contains(&true)).await;
        assert!(!statuses.lock().unwrap()[0]);
        let watched = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with("/api/stats/watchtime"))
            .filter_map(|l| query(l, "st").zip(query(l, "et")))
            .collect::<Vec<_>>();
        let last = watched.last().unwrap();
        assert_eq!(last, &("10.000".to_owned(), "15.000".to_owned()));
        drop(tracker);
        wait_until(|| client.pending.lock().unwrap().is_empty()).await;
    }

    #[tokio::test]
    async fn resume_lookups_wait_only_for_the_same_videos_reports() {
        let client = test_client();
        // Nothing pending: returns immediately.
        tokio::time::timeout(Duration::from_millis(100), client.settled("aqz-KE-bpKQ"))
            .await
            .unwrap();
        let pending = Pending::new(client.clone(), "aqz-KE-bpKQ");
        let again = Pending::new(client.clone(), "aqz-KE-bpKQ");
        tokio::time::timeout(Duration::from_millis(100), client.settled("other-video"))
            .await
            .unwrap();
        let waiting = tokio::spawn({
            let client = client.clone();
            async move { client.settled("aqz-KE-bpKQ").await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(pending);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished(), "another viewing is still reporting");
        drop(again);
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap();
        assert!(client.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn resume_and_tracking_metadata_are_validated() {
        let mut raw = json!({"videoDetails":{"lengthSeconds":"600", "isLiveContent":true},
            "playerConfig":{"playbackStartConfig":{"startSeconds":123.5}},
            "playbackTracking":{"videostatsPlaybackUrl":{"baseUrl":"https://s.youtube.com/api/stats/playback?docid=example"},
                "videostatsWatchtimeUrl":{"baseUrl":"https://www.youtube.com/api/stats/watchtime?docid=example"}}});
        let info = Info::parse(&raw);
        assert_eq!(info.resume, Some(123.5)); // archived live videos can resume
        assert_eq!(info.playback.unwrap().host_str(), Some("www.youtube.com"));
        raw["videoDetails"]["isLive"] = json!(true);
        assert!(Info::parse(&raw).resume.is_none());
        raw["videoDetails"]["isLive"] = json!(false);
        for invalid in [json!(-1), json!(600), json!("NaN")] {
            raw["playerConfig"]["playbackStartConfig"]["startSeconds"] = invalid;
            assert!(Info::parse(&raw).resume.is_none());
        }
        for url in [
            "https://www.youtube.com.evil/api/stats/watchtime",
            "http://www.youtube.com/api/stats/watchtime",
            "https://www.youtube.com/other",
            "https://user@www.youtube.com/api/stats/watchtime",
        ] {
            assert!(tracking_url(&json!(url), "watchtime").is_none());
        }
    }

    #[tokio::test]
    async fn progress_excludes_seek_gaps_and_pause_and_flushes_final_position() {
        let (mut tracker, mut receiver) = test_tracker();
        let now = Instant::now();
        tracker.observe(sample(10., false), now);
        assert!(receiver.try_recv().is_err()); // merely loading doesn't count as watched
        tracker.observe(sample(11., false), now + Duration::from_secs(1));
        let first = receiver.try_recv().unwrap();
        assert_eq!(first.segments, [(10., 11.)]);
        tracker.observe(sample(20., false), now + Duration::from_secs(10));
        tracker.seek(true);
        assert_eq!(receiver.try_recv().unwrap().segments, [(11., 20.)]);
        tracker.seek(false); // mpv's event for the same seek
        assert!(receiver.try_recv().is_err());
        tracker.observe(sample(20., false), now + Duration::from_millis(10100));
        tracker.observe(sample(20.5, false), now + Duration::from_millis(10200));
        assert!(tracker.segments.is_empty()); // even short seeks are not watched time
        assert!(receiver.try_recv().is_err());
        tracker.restarted();
        tracker.observe(sample(100., true), now + Duration::from_secs(11));
        let seek = receiver.try_recv().unwrap();
        assert_eq!(seek.position, 100.);
        assert!(seek.segments.is_empty());
        tracker.observe(sample(100., true), now + Duration::from_secs(60));
        assert!(receiver.try_recv().is_err()); // paused time creates no watch interval
        tracker.observe(sample(100., false), now + Duration::from_secs(61));
        assert!(receiver.try_recv().unwrap().playing);
        tracker.observe(sample(101., false), now + Duration::from_secs(62));
        let periodic = receiver.try_recv().unwrap();
        assert!(periodic.playing && !periodic.final_report);
        assert_eq!(periodic.segments, [(100., 101.)]);
        tracker.flush(true);
        let final_report = receiver.try_recv().unwrap();
        assert!(final_report.final_report);
        assert_eq!(final_report.position, 101.);
        assert!(!final_report.playing);
        drop(tracker);
        assert!(receiver.try_recv().is_err()); // EOF + close must not report twice
    }

    #[test]
    fn ping_replaces_query_fields_and_keeps_segments_separate() {
        let report = Report {
            position: 110.,
            duration: 600.,
            elapsed: 20.,
            playing: false,
            final_report: true,
            segments: vec![(0., 10.), (100., 110.)],
        };
        let url = report.url(
            &Url::parse(
                "https://www.youtube.com/api/stats/watchtime?docid=video&cmt=0&st=0&et=0&final=0",
            )
            .unwrap(),
            "test-nonce",
            true,
        );
        let pairs = url.query_pairs().collect::<Vec<_>>();
        assert_eq!(pairs.iter().filter(|(k, _)| k == "cmt").count(), 1);
        assert!(pairs.iter().any(|(k, v)| k == "st" && v == "0.000,100.000"));
        assert!(
            pairs
                .iter()
                .any(|(k, v)| k == "et" && v == "10.000,110.000")
        );
        assert!(pairs.iter().any(|(k, v)| k == "final" && v == "1"));
        assert_ne!(nonce(), nonce());
        assert_eq!(nonce().len(), 16);
    }

    #[tokio::test]
    #[ignore = "requires an explicitly authorized browser profile and network"]
    async fn live_playback_metadata() {
        let selection = crate::browser::Selection {
            browser: "chromium".into(),
            profile: std::env::var("SLINTYT_TEST_PROFILE")
                .expect("authorized profile required")
                .into(),
            keyring: None,
        };
        let (pipe, cookies, identity) = crate::youtube::authenticated_client(&selection)
            .await
            .expect("Authentication failed");
        let client = Client::new(pipe, &cookies, &identity).expect("Playback client failed");
        let mut id = std::env::var("SLINTYT_TEST_VIDEO").unwrap_or_else(|_| "aqz-KE-bpKQ".into());
        if std::env::var_os("SLINTYT_TEST_EXISTING").is_some() {
            let raw = client
                .pipe
                .query()
                .authenticated()
                .raw(
                    ClientType::Desktop,
                    "browse",
                    &json!({"browseId":"FEhistory"}),
                )
                .await
                .unwrap();
            fn pick(v: &Value) -> Option<String> {
                if let Some(r) = v.get("lockupViewModel")
                    && r.to_string().contains("startPercent")
                {
                    return r["contentId"]
                        .as_str()
                        .filter(|id| super::super::resolve::valid_id(id))
                        .map(str::to_owned);
                }
                if let Some(r) = v.get("videoRenderer")
                    && r.get("thumbnailOverlays")
                        .is_some_and(|v| v.to_string().contains("percentDurationWatched"))
                {
                    return r["videoId"].as_str().map(str::to_owned);
                }
                match v {
                    Value::Object(m) => m.values().find_map(pick),
                    Value::Array(a) => a.iter().find_map(pick),
                    _ => None,
                }
            }
            let mut found = pick(&serde_json::from_str::<Value>(&raw).unwrap());
            if found.is_none() {
                let raw = client
                    .pipe
                    .query()
                    .authenticated()
                    .raw(
                        ClientType::Desktop,
                        "browse",
                        &json!({"browseId":"FEwhat_to_watch"}),
                    )
                    .await
                    .unwrap();
                found = pick(&serde_json::from_str::<Value>(&raw).unwrap());
            }
            id = found.expect("No progress entry found");
        }
        let raw = client
            .pipe
            .query()
            .authenticated()
            .raw(ClientType::Desktop, "player", &json!({"videoId":id}))
            .await
            .expect("Metadata request failed");
        let v: Value = serde_json::from_str(&raw).unwrap();
        let info = Info::parse(&v);
        fn numeric_shape(value: &Value) -> Value {
            match value {
                Value::Object(m) => Value::Object(
                    m.iter()
                        .map(|(k, v)| (k.clone(), numeric_shape(v)))
                        .collect(),
                ),
                Value::Array(a) => Value::Array(a.iter().map(numeric_shape).collect()),
                Value::Number(_) => value.clone(),
                Value::String(s) => s.parse::<f64>().ok().map_or(json!("string"), |n| json!(n)),
                _ => json!("other"),
            }
        }
        println!(
            "Resume configuration {}",
            numeric_shape(&v["playerConfig"]["playbackStartConfig"])
        );
        println!(
            "Tracking playback={} watchtime={} resume={:?} duration={}",
            info.playback.is_some(),
            info.watchtime.is_some(),
            info.resume,
            info.duration
        );
        assert!(info.playback.is_some() && info.watchtime.is_some());
        println!(
            "Caption tracks={}",
            v.pointer("/captions/playerCaptionsTracklistRenderer/captionTracks")
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        );
    }
}
