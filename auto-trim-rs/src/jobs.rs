//! Background jobs: progress, cancellation and retention.
//!
//! A three-hour video takes minutes to trim, which is far too long to hold an HTTP
//! request open for. Work runs on its own thread and the caller polls this registry
//! instead. Progress is kept in atomics so a poll never has to wait on the worker,
//! and the weights make the reported fraction track *time* rather than frames — the
//! bar moves at a roughly even pace instead of crawling through the render.

use std::collections::HashMap;
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Decide the edit and report it, without rendering anything.
    Analyze,
    /// Decide the edit, then render the kept frames to a file.
    Trim,
    /// Line up two recordings by their sound, without cutting anything.
    Sync,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Analyze => "analyze",
            Mode::Trim => "trim",
            Mode::Sync => "sync",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Queued = 0,
    Analyzing = 1,
    Rendering = 2,
    Finishing = 3,
    Finished = 4,
}

impl Phase {
    fn from_u8(value: u8) -> Phase {
        match value {
            1 => Phase::Analyzing,
            2 => Phase::Rendering,
            3 => Phase::Finishing,
            4 => Phase::Finished,
            _ => Phase::Queued,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Queued => "queued",
            Phase::Analyzing => "analyzing",
            Phase::Rendering => "rendering",
            Phase::Finishing => "finishing",
            Phase::Finished => "finished",
        }
    }
}

/// How long each phase is expected to take, in seconds. Used purely to weight the
/// progress fraction, so a phase that takes most of the wall clock takes most of
/// the bar.
#[derive(Clone, Copy, Debug)]
pub struct PhaseWeights {
    pub analyze: f64,
    pub render: f64,
    pub finish: f64,
}

impl Default for PhaseWeights {
    fn default() -> Self {
        PhaseWeights {
            analyze: 1.0,
            render: 0.0,
            finish: 0.0,
        }
    }
}

#[derive(Default)]
pub struct Progress {
    phase: AtomicU8,
    analyzed_frames: AtomicU64,
    expected_frames: AtomicU64,
    rendered_frames: AtomicU64,
    kept_frames: AtomicU64,
    /// Audio is spliced and encoded alongside the picture; it only matters to the
    /// bar when there is no picture to render.
    audio_frames: AtomicU64,
    audio_total: AtomicU64,
    weights: Mutex<Option<PhaseWeights>>,
}

impl Progress {
    pub fn set_phase(&self, phase: Phase) {
        self.phase.store(phase as u8, Ordering::Relaxed);
    }

    pub fn phase(&self) -> Phase {
        Phase::from_u8(self.phase.load(Ordering::Relaxed))
    }

    pub fn set_weights(&self, weights: PhaseWeights) {
        if let Ok(mut slot) = self.weights.lock() {
            *slot = Some(weights);
        }
    }

    fn weights(&self) -> PhaseWeights {
        self.weights
            .lock()
            .ok()
            .and_then(|slot| *slot)
            .unwrap_or_default()
    }

    pub fn set_expected_frames(&self, frames: u64) {
        self.expected_frames.store(frames, Ordering::Relaxed);
    }

    pub fn add_analyzed_frames(&self, frames: u64) {
        self.analyzed_frames.fetch_add(frames, Ordering::Relaxed);
    }

    pub fn set_kept_frames(&self, frames: u64) {
        self.kept_frames.store(frames, Ordering::Relaxed);
    }

    pub fn add_rendered_frames(&self, frames: u64) {
        self.rendered_frames.fetch_add(frames, Ordering::Relaxed);
    }

    /// Adds rather than replaces: a pair is two renders inside one job, and the
    /// bar has to span both of them.
    pub fn add_audio_total(&self, samples: u64) {
        self.audio_total.fetch_add(samples, Ordering::Relaxed);
    }

    pub fn add_audio_samples(&self, samples: u64) {
        self.audio_frames.fetch_add(samples, Ordering::Relaxed);
    }

    fn ratio(done: u64, total: u64) -> f64 {
        if total == 0 {
            return 0.0;
        }
        (done as f64 / total as f64).clamp(0.0, 1.0)
    }

    /// How far along the whole job is, 0 to 1.
    pub fn fraction(&self) -> f64 {
        let phase = self.phase();
        if phase == Phase::Finished {
            return 1.0;
        }

        let weights = self.weights();
        let total = weights.analyze + weights.render + weights.finish;
        if total <= 0.0 {
            return 0.0;
        }

        let analyze = Self::ratio(
            self.analyzed_frames.load(Ordering::Relaxed),
            self.expected_frames.load(Ordering::Relaxed),
        );

        // Picture and audio encode at the same time, and the phase is over when
        // the slower of them is done — so the laggard is the progress. Taking the
        // faster one (or the greater of the two) would park the bar at 100% while
        // the other still had minutes to run. A part with nothing to do at all
        // (an audio-only input has no picture) is left out rather than counted
        // as zero forever.
        let kept_frames = self.kept_frames.load(Ordering::Relaxed);
        let audio_total = self.audio_total.load(Ordering::Relaxed);
        let mut render = 1.0f64;
        let mut has_render_work = false;
        if kept_frames > 0 {
            render = render.min(Self::ratio(
                self.rendered_frames.load(Ordering::Relaxed),
                kept_frames,
            ));
            has_render_work = true;
        }
        if audio_total > 0 {
            render = render.min(Self::ratio(
                self.audio_frames.load(Ordering::Relaxed),
                audio_total,
            ));
            has_render_work = true;
        }
        if !has_render_work {
            render = 0.0;
        }

        let done = match phase {
            Phase::Queued => 0.0,
            Phase::Analyzing => weights.analyze * analyze,
            Phase::Rendering => weights.analyze + weights.render * render,
            Phase::Finishing | Phase::Finished => weights.analyze + weights.render,
        };
        (done / total).clamp(0.0, 1.0)
    }

}

/// Why a phase stopped early. A cancel is a normal outcome, not a failure, and the
/// two must not be reported the same way.
pub enum WorkError {
    Canceled,
    Failed(String),
}

impl From<String> for WorkError {
    fn from(message: String) -> Self {
        WorkError::Failed(message)
    }
}

pub type WorkResult<T> = Result<T, WorkError>;

pub enum Outcome {
    Done(serde_json::Value),
    Failed(String),
    Canceled,
}

pub struct Job {
    pub id: String,
    pub mode: Mode,
    pub video_path: String,
    pub started: Instant,
    pub progress: Progress,
    /// The estimate made before the work started, for a bar that has nothing
    /// measured yet.
    pub estimate: Mutex<Option<serde_json::Value>>,
    /// The edit decision, published as soon as the analysis phase ends so the UI
    /// can show what will be cut while the render is still running.
    pub analysis: Mutex<Option<serde_json::Value>>,
    /// Every kept range, uncapped, for an export to ask for once.
    pub segments: Mutex<Option<serde_json::Value>>,
    canceled: AtomicBool,
    children: Mutex<Vec<Arc<Mutex<Child>>>>,
    outcome: Mutex<Option<Outcome>>,
    finished_at: Mutex<Option<Instant>>,
}

impl Job {
    pub fn is_canceled(&self) -> bool {
        self.canceled.load(Ordering::Relaxed)
    }

    /// Ask the job to stop and kill whatever ffmpeg it has running.
    pub fn cancel(&self) {
        self.canceled.store(true, Ordering::Relaxed);
        self.kill_children();
    }

    fn kill_children(&self) {
        if let Ok(children) = self.children.lock() {
            for child in children.iter() {
                if let Ok(mut child) = child.lock() {
                    let _ = child.kill();
                }
            }
        }
    }

    /// Track a child process so a cancel can reach it. A job that is already
    /// canceled kills the newcomer straight away.
    pub fn register_child(&self, child: Arc<Mutex<Child>>) {
        if self.is_canceled() {
            if let Ok(mut child) = child.lock() {
                let _ = child.kill();
            }
            return;
        }
        if let Ok(mut children) = self.children.lock() {
            children.push(child);
        }
    }

    pub fn forget_child(&self, child: &Arc<Mutex<Child>>) {
        if let Ok(mut children) = self.children.lock() {
            children.retain(|tracked| !Arc::ptr_eq(tracked, child));
        }
    }

    pub fn finish(&self, outcome: Outcome) {
        if let Ok(mut slot) = self.outcome.lock() {
            *slot = Some(outcome);
        }
        if let Ok(mut slot) = self.finished_at.lock() {
            *slot = Some(Instant::now());
        }
        self.progress.set_phase(Phase::Finished);
    }

    pub fn elapsed(&self) -> f64 {
        let finished = self.finished_at.lock().ok().and_then(|slot| *slot);
        match finished {
            Some(at) => at.duration_since(self.started).as_secs_f64(),
            None => self.started.elapsed().as_secs_f64(),
        }
    }

    /// (state, error, result) for a status response.
    pub fn snapshot(&self) -> (&'static str, Option<String>, Option<serde_json::Value>) {
        let outcome = self.outcome.lock();
        match outcome.as_ref().ok().and_then(|slot| slot.as_ref()) {
            Some(Outcome::Done(value)) => ("done", None, Some(value.clone())),
            Some(Outcome::Failed(message)) => ("failed", Some(message.clone()), None),
            Some(Outcome::Canceled) => ("canceled", None, None),
            None => {
                if self.is_canceled() {
                    ("canceling", None, None)
                } else {
                    ("running", None, None)
                }
            }
        }
    }

    pub fn is_finished(&self) -> bool {
        self.outcome
            .lock()
            .map(|slot| slot.is_some())
            .unwrap_or(false)
    }

    fn finished_for(&self) -> Option<Duration> {
        self.finished_at
            .lock()
            .ok()
            .and_then(|slot| *slot)
            .map(|at| at.elapsed())
    }
}

pub struct Jobs {
    map: Mutex<HashMap<String, Arc<Job>>>,
    counter: AtomicU64,
    retention: Duration,
    max_active: usize,
}

impl Jobs {
    pub fn new(retention: Duration, max_active: usize) -> Self {
        Jobs {
            map: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
            retention,
            max_active,
        }
    }

    /// Register a new job, or report how many are already running when the queue
    /// is full. A bounded count is what keeps a machine with several trims in
    /// flight from thrashing.
    pub fn create(&self, mode: Mode, video_path: String) -> Result<Arc<Job>, usize> {
        let mut map = self.map.lock().expect("job registry poisoned");
        map.retain(|_, job| match job.finished_for() {
            Some(age) => age < self.retention,
            None => true,
        });

        let active = map.values().filter(|job| !job.is_finished()).count();
        if active >= self.max_active {
            return Err(active);
        }

        let id = format!(
            "job-{}-{}",
            std::process::id(),
            self.counter.fetch_add(1, Ordering::Relaxed)
        );
        let job = Arc::new(Job {
            id: id.clone(),
            mode,
            video_path,
            started: Instant::now(),
            progress: Progress::default(),
            estimate: Mutex::new(None),
            analysis: Mutex::new(None),
            segments: Mutex::new(None),
            canceled: AtomicBool::new(false),
            children: Mutex::new(Vec::new()),
            outcome: Mutex::new(None),
            finished_at: Mutex::new(None),
        });
        map.insert(id, Arc::clone(&job));
        Ok(job)
    }

    pub fn get(&self, id: &str) -> Option<Arc<Job>> {
        self.map
            .lock()
            .expect("job registry poisoned")
            .get(id)
            .cloned()
    }
}

/// A plain counting semaphore, so encode slots can be held across blocking ffmpeg
/// runs without dragging the async runtime into it.
pub struct Slots {
    available: Mutex<usize>,
    ready: Condvar,
}

impl Slots {
    pub fn new(count: usize) -> Self {
        Slots {
            available: Mutex::new(count.max(1)),
            ready: Condvar::new(),
        }
    }

    pub fn acquire(&self) -> SlotGuard<'_> {
        let mut available = self.available.lock().expect("slots poisoned");
        while *available == 0 {
            available = self.ready.wait(available).expect("slots poisoned");
        }
        *available -= 1;
        SlotGuard { slots: self }
    }
}

pub struct SlotGuard<'a> {
    slots: &'a Slots,
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        let mut available = self.slots.available.lock().expect("slots poisoned");
        *available += 1;
        self.slots.ready.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bar_is_weighted_by_expected_time() {
        let progress = Progress::default();
        progress.set_weights(PhaseWeights {
            analyze: 10.0,
            render: 90.0,
            finish: 0.0,
        });
        progress.set_expected_frames(100);
        progress.set_kept_frames(100);

        progress.set_phase(Phase::Analyzing);
        progress.add_analyzed_frames(50);
        // Half of a phase worth a tenth of the job.
        assert!((progress.fraction() - 0.05).abs() < 1e-9);

        progress.set_phase(Phase::Rendering);
        progress.add_rendered_frames(50);
        assert!((progress.fraction() - 0.55).abs() < 1e-9);

        progress.set_phase(Phase::Finished);
        assert_eq!(progress.fraction(), 1.0);
    }

    #[test]
    fn audio_only_work_still_moves_the_bar() {
        let progress = Progress::default();
        progress.set_weights(PhaseWeights {
            analyze: 0.0,
            render: 1.0,
            finish: 0.0,
        });
        progress.set_phase(Phase::Rendering);
        progress.add_audio_total(200);
        progress.add_audio_samples(50);
        assert!((progress.fraction() - 0.25).abs() < 1e-9);
    }

    #[test]
    fn the_slower_half_of_the_render_sets_the_pace() {
        // The picture is done and the audio is a third of the way in: the phase
        // is a third done, not finished.
        let progress = Progress::default();
        progress.set_weights(PhaseWeights {
            analyze: 0.0,
            render: 1.0,
            finish: 0.0,
        });
        progress.set_phase(Phase::Rendering);
        progress.set_kept_frames(100);
        progress.add_rendered_frames(100);
        progress.add_audio_total(300);
        progress.add_audio_samples(100);
        assert!(
            (progress.fraction() - 1.0 / 3.0).abs() < 1e-9,
            "got {}",
            progress.fraction()
        );
    }

    #[test]
    fn the_queue_is_bounded() {
        let jobs = Jobs::new(Duration::from_secs(60), 2);
        let first = jobs.create(Mode::Trim, "a.mp4".to_string()).expect("first");
        let _second = jobs.create(Mode::Trim, "b.mp4".to_string()).expect("second");
        assert!(jobs.create(Mode::Trim, "c.mp4".to_string()).is_err());

        // Finishing one frees a slot.
        first.finish(Outcome::Canceled);
        assert!(jobs.create(Mode::Trim, "c.mp4".to_string()).is_ok());
    }

    #[test]
    fn finished_jobs_stay_available_for_polling() {
        let jobs = Jobs::new(Duration::from_secs(60), 1);
        let job = jobs.create(Mode::Analyze, "a.mp4".to_string()).expect("job");
        job.finish(Outcome::Done(serde_json::json!({"ok": true})));

        let found = jobs.get(&job.id).expect("still registered");
        let (state, error, result) = found.snapshot();
        assert_eq!(state, "done");
        assert!(error.is_none());
        assert_eq!(result.unwrap()["ok"], true);
    }

    #[test]
    fn slots_hand_out_no_more_than_they_have() {
        let slots = Slots::new(1);
        let guard = slots.acquire();
        drop(guard);
        let _second = slots.acquire();
    }
}
