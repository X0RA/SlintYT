//! YouTube's seek-preview sprite sheets, read from the same yt-dlp response as
//! the stream catalogue. Sheet URLs are signed, so they stay in memory only.
use serde_json::Value;
use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::task::JoinHandle;

const MAX_FRAME: u32 = 1280;
const MAX_GRID: u32 = 32;
const MAX_SHEET: u32 = 4096;
const MAX_PAGES: usize = 20_000;
/// A sheet takes hundreds of milliseconds to arrive, too slow to fetch on hover,
/// so the whole video loads ahead of the pointer. That is about 1 MB per hour.
const MAX_FETCHES: usize = 6;
const MAX_COMPRESSED_BYTES: usize = 64 * 1024 * 1024;
/// A decoded sheet is about 2 MB, so only those near the pointer are kept.
const MAX_DECODES: usize = 4;
const MAX_DECODED_BYTES: usize = 64 * 1024 * 1024;
const RETRY_AFTER: Duration = Duration::from_secs(30);

pub type Sheet = SharedPixelBuffer<Rgba8Pixel>;

#[derive(Clone, Debug, PartialEq)]
pub struct Storyboard {
    frame_width: u32,
    frame_height: u32,
    columns: u32,
    rows: u32,
    frames: u32,
    seconds_per_frame: f64,
    pages: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub page: usize,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Every usable quality level, sharpest first. Parsed apart from the catalogue,
/// so an unexpected storyboard field only costs the previews and never the
/// quality menu. Live streams have none.
pub fn parse(bytes: &[u8]) -> Vec<Storyboard> {
    let Ok(info) = serde_json::from_slice::<Value>(bytes) else {
        return Vec::new();
    };
    let Some(duration) = info["duration"]
        .as_f64()
        .filter(|d| d.is_finite() && *d > 0.)
    else {
        return Vec::new();
    };
    let mut levels: Vec<_> = info["formats"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|format| level(format, duration))
        .collect();
    levels.sort_by_key(|level| std::cmp::Reverse(level.frame_width * level.frame_height));
    levels
}

/// A video may offer fewer levels than the settings do, so anything past its
/// last level uses that last one, which is the closest match.
pub fn pick(levels: &[Arc<Storyboard>], rank: usize) -> Option<Arc<Storyboard>> {
    levels.get(rank).or(levels.last()).cloned()
}

fn level(format: &Value, duration: f64) -> Option<Storyboard> {
    if !format["format_id"].as_str()?.starts_with("sb") {
        return None;
    }
    let number = |key: &str| {
        format[key]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0)
    };
    let frame_width = number("width").filter(|n| *n <= MAX_FRAME)?;
    let frame_height = number("height").filter(|n| *n <= MAX_FRAME)?;
    let columns = number("columns").filter(|n| *n <= MAX_GRID)?;
    let rows = number("rows").filter(|n| *n <= MAX_GRID)?;
    if frame_width * columns > MAX_SHEET || frame_height * rows > MAX_SHEET {
        return None;
    }
    let fps = format["fps"]
        .as_f64()
        .filter(|f| f.is_finite() && *f > 0.)?;
    let pages = format["fragments"]
        .as_array()?
        .iter()
        .map(|page| {
            page["url"]
                .as_str()
                .filter(|url| allowed(url))
                .map(str::to_owned)
        })
        .collect::<Option<Vec<_>>>()?;
    if pages.is_empty() || pages.len() > MAX_PAGES {
        return None;
    }
    let capacity = pages.len() as u32 * columns * rows;
    Some(Storyboard {
        frame_width,
        frame_height,
        columns,
        rows,
        frames: ((fps * duration).round() as u32).clamp(1, capacity),
        seconds_per_frame: 1. / fps,
        pages,
    })
}

fn allowed(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| {
        url.scheme() == "https"
            && url
                .host_str()
                .is_some_and(|h| h == "i.ytimg.com" || h.ends_with(".ytimg.com"))
    })
}

impl Storyboard {
    /// Cell sizes come from the metadata and never from the decoded image:
    /// a video's last sheet only has room for its remaining frames.
    pub fn locate(&self, seconds: f32) -> Option<Frame> {
        if !seconds.is_finite() {
            return None;
        }
        let index = (f64::from(seconds.max(0.)) / self.seconds_per_frame) as u32;
        let index = index.min(self.frames - 1);
        let per_page = self.columns * self.rows;
        let cell = index % per_page;
        Some(Frame {
            page: (index / per_page) as usize,
            x: cell % self.columns * self.frame_width,
            y: cell / self.columns * self.frame_height,
            width: self.frame_width,
            height: self.frame_height,
        })
    }

    /// No genuine sheet is larger than a full grid of frames.
    pub fn sheet_limit(&self) -> (u32, u32) {
        (
            self.columns * self.frame_width,
            self.rows * self.frame_height,
        )
    }

    /// How many sheets fit in the decoded budget.
    fn decoded_capacity(&self) -> usize {
        let (width, height) = self.sheet_limit();
        (MAX_DECODED_BYTES / (width as usize * height as usize * 4)).max(1)
    }
}

/// What the preview should show for the hovered position.
pub enum View {
    /// Leave it as it is, including the previous frame while a sheet loads.
    Wait,
    /// No frame is available for this position, so only the time shows.
    Hide,
    /// `sheet` is set only when the page changed, to skip a texture upload.
    Show { frame: Frame, sheet: Option<Sheet> },
}

/// Work to run off the UI thread, reporting back with its ticket.
pub enum Job {
    Fetch {
        ticket: u64,
        page: usize,
        url: String,
    },
    Decode {
        ticket: u64,
        page: usize,
        bytes: Arc<Vec<u8>>,
        limit: (u32, u32),
    },
}

pub struct Update {
    pub view: View,
    pub jobs: Vec<Job>,
}

struct Running {
    ticket: u64,
    decode: bool,
    task: Option<JoinHandle<()>>,
}

/// Previews for the current video. All of it runs on the UI thread except the
/// jobs, whose results are matched back through a ticket. Aborting a task cannot
/// recall a result it already posted, so an unknown ticket is ignored. Jobs are
/// never dropped because the pointer moved: each finished sheet is one the pan
/// may need next, and aborting would also close its connection.
#[derive(Default)]
pub struct Preview {
    storyboard: Option<Arc<Storyboard>>,
    compressed: HashMap<usize, Arc<Vec<u8>>>,
    compressed_bytes: usize,
    /// Set for good at the limit: evicted sheets would otherwise look missing,
    /// and background loading would fetch them again forever.
    compressed_full: bool,
    decoded: HashMap<usize, Sheet>,
    failed: HashMap<usize, Instant>,
    shown: Option<usize>,
    hovered: Option<f32>,
    /// Nothing is decoded until the first hover, as most videos never see one.
    decoding: bool,
    focus: usize,
    next_ticket: u64,
    running: HashMap<usize, Running>,
}

impl Preview {
    pub fn hover(&mut self, seconds: f32) -> Update {
        self.hovered = Some(seconds);
        self.decoding = true;
        if let Some(frame) = self.storyboard.as_ref().and_then(|b| b.locate(seconds)) {
            self.focus = frame.page;
        }
        self.update()
    }

    pub fn leave(&mut self) {
        self.hovered = None;
    }

    /// Starts loading from the playback position. The hover is re-evaluated
    /// because a pointer resting on the bar sends nothing when metadata arrives.
    pub fn set_storyboard(&mut self, storyboard: Option<Arc<Storyboard>>, position: f32) -> Update {
        if self.storyboard.as_deref() != storyboard.as_deref() {
            self.clear();
            self.storyboard = storyboard;
            self.decoding = self.hovered.is_some();
            let focus = self.hovered.unwrap_or(position);
            if let Some(frame) = self.storyboard.as_ref().and_then(|b| b.locate(focus)) {
                self.focus = frame.page;
            }
        }
        self.update()
    }

    /// Stops the jobs but keeps the sheets, for reloading the same video.
    pub fn pause(&mut self) {
        for (_, running) in self.running.drain() {
            if let Some(task) = running.task {
                task.abort();
            }
        }
    }

    /// Keeps the hover, so the next video's storyboard can restore its preview.
    pub fn clear(&mut self) {
        self.pause();
        self.storyboard = None;
        self.compressed.clear();
        self.compressed_bytes = 0;
        self.compressed_full = false;
        self.decoded.clear();
        self.failed.clear();
        self.shown = None;
        self.decoding = false;
        self.focus = 0;
    }

    pub fn started(&mut self, ticket: u64, page: usize, task: JoinHandle<()>) {
        match self.running.get_mut(&page).filter(|r| r.ticket == ticket) {
            Some(running) => running.task = Some(task),
            None => task.abort(),
        }
    }

    pub fn fetched(&mut self, ticket: u64, page: usize, bytes: Option<Vec<u8>>) -> Update {
        if !self.finish(ticket, page) {
            return self.nothing();
        }
        match bytes {
            Some(bytes) => {
                self.compressed_bytes += bytes.len();
                self.compressed.insert(page, Arc::new(bytes));
                self.compressed_full |= self.compressed_bytes >= MAX_COMPRESSED_BYTES;
                while self.compressed_bytes > MAX_COMPRESSED_BYTES {
                    let Some(far) = farthest(self.compressed.keys(), self.focus, page) else {
                        break;
                    };
                    if let Some(bytes) = self.compressed.remove(&far) {
                        self.compressed_bytes -= bytes.len();
                    }
                }
            }
            None => self.fail(page),
        }
        self.update()
    }

    pub fn decoded(&mut self, ticket: u64, page: usize, sheet: Option<Sheet>) -> Update {
        if !self.finish(ticket, page) {
            return self.nothing();
        }
        match sheet {
            Some(sheet) => {
                self.decoded.insert(page, sheet);
                let capacity = self
                    .storyboard
                    .as_ref()
                    .map_or(1, |board| board.decoded_capacity());
                while self.decoded.len() > capacity {
                    let Some(far) = farthest(self.decoded.keys(), self.focus, self.focus) else {
                        break;
                    };
                    self.decoded.remove(&far);
                }
            }
            None => {
                // The download may be what is broken, so the next try fetches again.
                if let Some(bytes) = self.compressed.remove(&page) {
                    self.compressed_bytes -= bytes.len();
                }
                self.fail(page);
            }
        }
        self.update()
    }

    fn finish(&mut self, ticket: u64, page: usize) -> bool {
        if self.running.get(&page).is_none_or(|r| r.ticket != ticket) {
            return false;
        }
        self.running.remove(&page);
        true
    }

    fn fail(&mut self, page: usize) {
        self.failed.insert(page, Instant::now() + RETRY_AFTER);
    }

    fn nothing(&self) -> Update {
        Update {
            view: View::Wait,
            jobs: Vec::new(),
        }
    }

    fn update(&mut self) -> Update {
        Update {
            view: self.view(),
            jobs: self.schedule(),
        }
    }

    fn view(&mut self) -> View {
        let Some(seconds) = self.hovered else {
            return View::Wait;
        };
        let Some(frame) = self
            .storyboard
            .as_ref()
            .and_then(|board| board.locate(seconds))
        else {
            return View::Hide;
        };
        if let Some(sheet) = self.decoded.get(&frame.page) {
            if frame.x + frame.width > sheet.width() || frame.y + frame.height > sheet.height() {
                return View::Hide;
            }
            let changed = self.shown != Some(frame.page);
            let sheet = changed.then(|| sheet.clone());
            self.shown = Some(frame.page);
            return View::Show { frame, sheet };
        }
        let now = Instant::now();
        if self.failed.get(&frame.page).is_some_and(|r| now < *r) {
            return View::Hide;
        }
        View::Wait
    }

    /// Starts whatever is nearest the pointer, within the limits on running jobs.
    fn schedule(&mut self) -> Vec<Job> {
        let mut jobs = Vec::new();
        let Some(board) = self.storyboard.clone() else {
            return jobs;
        };
        let now = Instant::now();
        self.failed.retain(|_, retry| now < *retry);
        // A failure suggests the rest would fail too: expired URLs, or no network.
        let paused = !self.failed.is_empty();
        let mut fetches =
            MAX_FETCHES.saturating_sub(self.running.values().filter(|r| !r.decode).count());
        let mut decodes =
            MAX_DECODES.saturating_sub(self.running.values().filter(|r| r.decode).count());
        let radius = (board.decoded_capacity() - 1) / 2;
        for page in outward(self.focus, board.pages.len()) {
            if fetches == 0 && (decodes == 0 || !self.decoding) {
                break;
            }
            if self.running.contains_key(&page)
                || self.decoded.contains_key(&page)
                || self.failed.contains_key(&page)
            {
                continue;
            }
            self.next_ticket += 1;
            let ticket = self.next_ticket;
            if let Some(bytes) = self.compressed.get(&page) {
                if !self.decoding || decodes == 0 || page.abs_diff(self.focus) > radius {
                    continue;
                }
                decodes -= 1;
                jobs.push(Job::Decode {
                    ticket,
                    page,
                    bytes: bytes.clone(),
                    limit: board.sheet_limit(),
                });
            } else {
                let background = !self.compressed_full && !paused;
                if fetches == 0 || !(background || page == self.focus) {
                    continue;
                }
                fetches -= 1;
                jobs.push(Job::Fetch {
                    ticket,
                    page,
                    url: board.pages[page].clone(),
                });
            }
            self.running.insert(
                page,
                Running {
                    ticket,
                    decode: matches!(jobs.last(), Some(Job::Decode { .. })),
                    task: None,
                },
            );
        }
        jobs
    }
}

/// Every page, starting at `focus` and alternating outwards.
fn outward(focus: usize, pages: usize) -> impl Iterator<Item = usize> {
    (0..pages).flat_map(move |distance| {
        let after = (focus + distance < pages).then_some(focus + distance);
        let before = (distance > 0 && distance <= focus).then(|| focus - distance);
        after.into_iter().chain(before)
    })
}

fn farthest<'a>(
    pages: impl Iterator<Item = &'a usize>,
    focus: usize,
    keep: usize,
) -> Option<usize> {
    pages
        .copied()
        .filter(|page| *page != keep)
        .max_by_key(|page| page.abs_diff(focus))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sb(id: &str, width: u32, height: u32, columns: u32, rows: u32, pages: usize) -> Value {
        let fragments: Vec<_> = (0..pages)
            .map(
                |page| json!({"url": format!("https://i.ytimg.com/sb/id/{id}/M{page}.jpg?sigh=x")}),
            )
            .collect();
        json!({"format_id": id, "vcodec": "none", "width": width, "height": height,
            "columns": columns, "rows": rows, "fps": 0.1, "fragments": fragments})
    }

    fn info(formats: Vec<Value>) -> Vec<u8> {
        serde_json::to_vec(&json!({"duration": 2750, "formats": formats})).unwrap()
    }

    /// 275 frames at one per 10 seconds, on 31 sheets of 3x3.
    fn board() -> Storyboard {
        parse(&info(vec![sb("sb0", 320, 180, 3, 3, 31)])).remove(0)
    }

    fn sheet(width: u32, height: u32) -> Sheet {
        SharedPixelBuffer::new(width, height)
    }

    fn pages(jobs: &[Job]) -> Vec<usize> {
        jobs.iter()
            .map(|job| match job {
                Job::Fetch { page, .. } | Job::Decode { page, .. } => *page,
            })
            .collect()
    }

    /// Finishes every job, repeating until none remain. Sheets are full size.
    fn drain(preview: &mut Preview, mut jobs: Vec<Job>) {
        while let Some(job) = jobs.pop() {
            let update = match job {
                Job::Fetch { ticket, page, .. } => preview.fetched(ticket, page, Some(vec![0; 10])),
                Job::Decode { ticket, page, .. } => {
                    preview.decoded(ticket, page, Some(sheet(960, 540)))
                }
            };
            jobs.extend(update.jobs);
        }
    }

    fn preview() -> (Preview, Vec<Job>) {
        let mut preview = Preview::default();
        let update = preview.set_storyboard(Some(Arc::new(board())), 0.);
        (preview, update.jobs)
    }

    #[test]
    fn keeps_every_valid_level_sharpest_first() {
        let levels = parse(&info(vec![
            sb("sb3", 48, 27, 10, 10, 3),
            sb("sb0", 320, 180, 3, 3, 31),
            sb("sb1", 160, 90, 5, 5, 11),
            json!({"format_id": "137", "vcodec": "avc1", "width": 1920, "height": 1080}),
        ]));
        let sizes: Vec<_> = levels.iter().map(|l| l.frame_width).collect();
        assert_eq!(sizes, [320, 160, 48]);
        assert_eq!(levels[0].frames, 275);
    }

    #[test]
    fn a_missing_level_falls_back_to_the_closest_one() {
        let levels: Vec<_> = parse(&info(vec![
            sb("sb0", 320, 180, 3, 3, 31),
            sb("sb1", 160, 90, 5, 5, 11),
        ]))
        .into_iter()
        .map(Arc::new)
        .collect();
        let width = |rank| pick(&levels, rank).unwrap().frame_width;
        assert_eq!(
            (width(0), width(1), width(2), width(3)),
            (320, 160, 160, 160)
        );
        assert!(pick(&[], 0).is_none());
    }

    #[test]
    fn one_bad_level_leaves_the_others() {
        let mut bad = sb("sb0", 320, 180, 3, 3, 31);
        bad["rows"] = json!("three");
        let levels = parse(&info(vec![bad, sb("sb1", 160, 90, 5, 5, 11)]));
        assert_eq!(levels.len(), 1);
        assert_eq!(levels[0].frame_width, 160);
    }

    #[test]
    fn rejects_unusable_storyboards() {
        assert!(parse(b"not json").is_empty());
        assert!(parse(&info(vec![])).is_empty());
        let live = json!({"formats": [sb("sb0", 320, 180, 3, 3, 1)]});
        assert!(parse(&serde_json::to_vec(&live).unwrap()).is_empty());
        for (field, value) in [
            ("fps", json!(0)),
            ("fps", json!("fast")),
            ("rows", json!(0)),
            ("columns", json!(3.5)),
            ("width", json!(-1)),
            ("height", json!(100_000)),
            ("fragments", json!([])),
            ("fragments", json!([{"url": "http://i.ytimg.com/a.jpg"}])),
            (
                "fragments",
                json!([{"url": "https://ytimg.com.example/a.jpg"}]),
            ),
            ("fragments", json!([{"duration": 10}])),
        ] {
            let mut format = sb("sb0", 320, 180, 3, 3, 1);
            format[field] = value;
            assert!(parse(&info(vec![format])).is_empty(), "{field}");
        }
    }

    #[test]
    fn locates_frames_across_sheets() {
        let board = board();
        let frame = |seconds| board.locate(seconds).unwrap();
        let position = |f: Frame| (f.page, f.x, f.y);
        assert_eq!(position(frame(0.)), (0, 0, 0));
        assert_eq!(position(frame(-5.)), (0, 0, 0));
        assert_eq!(position(frame(19.)), (0, 320, 0));
        assert_eq!(position(frame(30.)), (0, 0, 180));
        assert_eq!(position(frame(89.)), (0, 640, 360));
        assert_eq!(position(frame(90.)), (1, 0, 0));
        assert!(board.locate(f32::NAN).is_none());
        assert!(board.locate(f32::INFINITY).is_none());
    }

    #[test]
    fn the_end_of_the_video_stays_on_the_last_real_frame() {
        let board = board();
        // Frame 274 is the fifth cell on sheet 30, whose image is only 960x360.
        for seconds in [2749., 2750., 99_999.] {
            assert_eq!(
                board.locate(seconds).unwrap(),
                Frame {
                    page: 30,
                    x: 320,
                    y: 180,
                    width: 320,
                    height: 180,
                }
            );
        }
    }

    #[test]
    fn turning_previews_off_stops_loading_and_forgets_the_sheets() {
        let (mut preview, jobs) = preview();
        drain(&mut preview, jobs);
        let jobs = preview.hover(0.).jobs;
        drain(&mut preview, jobs);
        assert!(!preview.decoded.is_empty());
        let update = preview.set_storyboard(None, 0.);
        assert!(update.jobs.is_empty());
        assert!(matches!(update.view, View::Hide));
        assert!(preview.compressed.is_empty() && preview.decoded.is_empty());
        assert!(preview.running.is_empty());
        // Turning them back on loads again, starting under the resting pointer.
        let update = preview.set_storyboard(Some(Arc::new(board())), 2000.);
        assert_eq!(pages(&update.jobs)[0], 0);
    }

    #[test]
    fn changing_quality_loads_the_new_level_instead() {
        let (mut preview, jobs) = preview();
        drain(&mut preview, jobs);
        let levels = parse(&info(vec![sb("sb1", 160, 90, 5, 5, 11)]));
        let update = preview.set_storyboard(Some(Arc::new(levels[0].clone())), 0.);
        assert!(preview.compressed.is_empty());
        assert_eq!(pages(&update.jobs), [0, 1, 2, 3, 4, 5]);
        drain(&mut preview, update.jobs);
        assert_eq!(preview.compressed.len(), 11);
    }

    #[test]
    fn walks_outwards_from_the_focus() {
        assert_eq!(outward(2, 5).collect::<Vec<_>>(), [2, 3, 1, 4, 0]);
        assert_eq!(outward(0, 3).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(outward(2, 3).collect::<Vec<_>>(), [2, 1, 0]);
        assert_eq!(outward(0, 0).count(), 0);
    }

    #[test]
    fn the_whole_video_loads_before_any_hover_without_decoding() {
        let (mut preview, jobs) = preview();
        assert_eq!(pages(&jobs), [0, 1, 2, 3, 4, 5]);
        assert!(jobs.iter().all(|job| matches!(job, Job::Fetch { .. })));
        drain(&mut preview, jobs);
        assert_eq!(preview.compressed.len(), 31);
        assert!(preview.running.is_empty());
        assert!(preview.decoded.is_empty());
    }

    #[test]
    fn loading_starts_at_the_playback_position() {
        let mut preview = Preview::default();
        let jobs = preview.set_storyboard(Some(Arc::new(board())), 1000.).jobs;
        assert_eq!(pages(&jobs), [11, 12, 10, 13, 9, 14]);
    }

    #[test]
    fn panning_never_cancels_running_downloads() {
        let (mut preview, jobs) = preview();
        let first = pages(&jobs);
        // Hovering further along takes nothing away from what is already running.
        let update = preview.hover(2000.);
        assert!(update.jobs.is_empty());
        for page in &first {
            assert!(preview.running.contains_key(page));
        }
        // The next free slot goes to the sheet under the pointer.
        let Job::Fetch { ticket, page, .. } = &jobs[0] else {
            panic!("expected a fetch");
        };
        let update = preview.fetched(*ticket, *page, Some(vec![0; 10]));
        assert_eq!(pages(&update.jobs), [22]);
    }

    #[test]
    fn a_decoded_sheet_shows_at_once_and_only_uploads_when_the_page_changes() {
        let (mut preview, jobs) = preview();
        drain(&mut preview, jobs);
        let update = preview.hover(40.);
        assert!(matches!(update.view, View::Wait));
        // The first decode to finish for this page brings the image with it.
        let (mut jobs, mut first) = (update.jobs, None);
        while first.is_none() {
            let Some(Job::Decode { ticket, page, .. }) = jobs.pop() else {
                panic!("expected a decode");
            };
            let update = preview.decoded(ticket, page, Some(sheet(960, 540)));
            jobs.extend(update.jobs);
            if let View::Show { frame, sheet } = update.view {
                first = Some((frame, sheet.is_some()));
            }
        }
        let (frame, uploaded) = first.unwrap();
        assert_eq!((frame.page, frame.x, frame.y), (0, 320, 180));
        assert!(uploaded);
        drain(&mut preview, jobs);
        // Only the clip changes on that sheet, with no decode wait on the next.
        assert!(matches!(
            preview.hover(50.).view,
            View::Show { sheet: None, .. }
        ));
        assert!(matches!(
            preview.hover(100.).view,
            View::Show { sheet: Some(_), .. }
        ));
    }

    #[test]
    fn decoded_sheets_stay_within_budget_and_near_the_pointer() {
        // 100 sheets of 960x540, against room for 32 of them.
        let mut preview = Preview::default();
        let mut long = board();
        long.pages = (0..100)
            .map(|page| format!("https://i.ytimg.com/{page}"))
            .collect();
        long.frames = 900;
        let capacity = long.decoded_capacity();
        assert_eq!(capacity, 32);
        let jobs = preview.set_storyboard(Some(Arc::new(long)), 0.).jobs;
        drain(&mut preview, jobs);
        assert_eq!(preview.compressed.len(), 100);
        let jobs = preview.hover(0.).jobs;
        drain(&mut preview, jobs);
        assert!(preview.decoded.len() <= capacity);
        assert!(preview.decoded.contains_key(&0));
        assert!(!preview.decoded.contains_key(&50));
        // Moving decodes around the new position, and sheets far behind go first.
        let jobs = preview.hover(4500.).jobs;
        drain(&mut preview, jobs);
        assert!(preview.decoded.len() <= capacity);
        assert!(preview.decoded.contains_key(&50));
        assert!(!preview.decoded.contains_key(&0));
    }

    #[test]
    fn compressed_sheets_are_bounded_too() {
        let (mut preview, jobs) = preview();
        let Job::Fetch { ticket, page, .. } = &jobs[0] else {
            panic!("expected a fetch");
        };
        let update = preview.fetched(*ticket, *page, Some(vec![0; MAX_COMPRESSED_BYTES]));
        // Being full stops further loading, except for what the pointer needs.
        assert!(update.jobs.is_empty());
        let Job::Fetch { ticket, page, .. } = &jobs[1] else {
            panic!("expected a fetch");
        };
        preview.fetched(*ticket, *page, Some(vec![0; 10]));
        assert!(preview.compressed_bytes <= MAX_COMPRESSED_BYTES);
        assert!(preview.compressed.contains_key(page));
    }

    #[test]
    fn stale_results_cannot_touch_newer_state() {
        let (mut preview, jobs) = preview();
        let Job::Fetch { ticket, page, .. } = &jobs[0] else {
            panic!("expected a fetch");
        };
        let (ticket, page) = (*ticket, *page);
        // A result from the previous storyboard is not filed under the new one.
        let mut other = board();
        other.frame_width = 240;
        let update = preview.set_storyboard(Some(Arc::new(other)), 0.);
        assert_eq!(pages(&update.jobs), [0, 1, 2, 3, 4, 5]);
        let update = preview.fetched(ticket, page, Some(vec![0; 10]));
        assert!(update.jobs.is_empty());
        assert!(preview.compressed.is_empty());
        let update = preview.fetched(ticket, page, None);
        assert!(update.jobs.is_empty());
        assert!(preview.failed.is_empty());
        let update = preview.decoded(ticket, page, Some(sheet(960, 540)));
        assert!(update.jobs.is_empty());
        assert!(preview.decoded.is_empty());
    }

    #[test]
    fn a_stationary_hover_is_restored_when_metadata_arrives() {
        let mut preview = Preview::default();
        assert!(matches!(preview.hover(0.).view, View::Hide));
        let update = preview.set_storyboard(Some(Arc::new(board())), 1000.);
        // The pointer outranks the playback position.
        assert_eq!(pages(&update.jobs)[0], 0);
        // The pointer never moves again, so its sheet must decode without a hover.
        drain(&mut preview, update.jobs);
        assert!(preview.decoded.contains_key(&0));
        assert!(matches!(preview.hover(0.).view, View::Show { .. }));
    }

    #[test]
    fn reaching_the_compressed_limit_stops_background_loading() {
        let (mut preview, mut jobs) = preview();
        let mut fetches = 0;
        while let Some(job) = jobs.pop() {
            let Job::Fetch { ticket, page, .. } = job else {
                panic!("expected a fetch");
            };
            fetches += 1;
            assert!(fetches < 31, "Evicted sheets were fetched again");
            let bytes = vec![0; MAX_COMPRESSED_BYTES / 3];
            jobs.extend(preview.fetched(ticket, page, Some(bytes)).jobs);
        }
        assert!(preview.compressed_bytes <= MAX_COMPRESSED_BYTES);
        // The sheet under the pointer still loads, and nothing follows it.
        let jobs = preview.hover(2000.).jobs;
        assert_eq!(pages(&jobs), [22]);
        let Job::Fetch { ticket, page, .. } = &jobs[0] else {
            panic!("expected a fetch");
        };
        let update = preview.fetched(*ticket, *page, Some(vec![0; 10]));
        assert!(preview.compressed.contains_key(page));
        assert!(
            update
                .jobs
                .iter()
                .all(|job| matches!(job, Job::Decode { .. }))
        );
    }

    #[test]
    fn reloading_the_same_video_keeps_its_sheets() {
        let (mut preview, jobs) = preview();
        let Job::Fetch { ticket, page, .. } = &jobs[0] else {
            panic!("expected a fetch");
        };
        let (ticket, page) = (*ticket, *page);
        preview.fetched(ticket, page, Some(vec![0; 10]));
        preview.pause();
        assert!(preview.running.is_empty());
        // The same storyboard arriving again resumes rather than starting over.
        let update = preview.set_storyboard(Some(Arc::new(board())), 0.);
        assert_eq!(preview.compressed.len(), 1);
        assert!(!pages(&update.jobs).contains(&page));
        assert_eq!(update.jobs.len(), MAX_FETCHES);
    }

    #[test]
    fn a_failure_pauses_loading_except_under_the_pointer() {
        let (mut preview, jobs) = preview();
        for job in &jobs {
            let Job::Fetch { ticket, page, .. } = job else {
                panic!("expected a fetch");
            };
            let update = preview.fetched(*ticket, *page, None);
            assert!(update.jobs.is_empty());
        }
        assert!(matches!(preview.hover(0.).view, View::Hide));
        // A healthy sheet under the pointer is still worth trying.
        let update = preview.hover(1000.);
        assert_eq!(pages(&update.jobs), [11]);
        // Once the failures expire, loading carries on.
        preview
            .failed
            .values_mut()
            .for_each(|retry| *retry = Instant::now());
        let Job::Fetch { ticket, page, .. } = &update.jobs[0] else {
            panic!("expected a fetch");
        };
        let update = preview.fetched(*ticket, *page, Some(vec![0; 10]));
        assert!(!update.jobs.is_empty());
    }

    #[test]
    fn a_broken_sheet_is_downloaded_again_after_the_wait() {
        let (mut preview, jobs) = preview();
        drain(&mut preview, jobs);
        let update = preview.hover(0.);
        let Some(Job::Decode { ticket, page, .. }) = update.jobs.first() else {
            panic!("expected a decode");
        };
        preview.decoded(*ticket, *page, None);
        assert!(!preview.compressed.contains_key(&0));
        assert!(matches!(preview.hover(0.).view, View::Hide));
        preview.failed.insert(0, Instant::now());
        assert!(pages(&preview.hover(0.).jobs).contains(&0));
    }

    #[test]
    fn a_cell_outside_a_partial_sheet_shows_no_frame() {
        let (mut preview, jobs) = preview();
        drain(&mut preview, jobs);
        let jobs = preview.hover(0.).jobs;
        for job in jobs {
            let Job::Decode { ticket, page, .. } = job else {
                panic!("expected a decode");
            };
            preview.decoded(ticket, page, Some(sheet(960, 180)));
        }
        assert!(matches!(preview.hover(0.).view, View::Show { .. }));
        assert!(matches!(preview.hover(30.).view, View::Hide));
    }
}
