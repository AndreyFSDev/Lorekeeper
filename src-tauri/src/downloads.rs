//! Model download queue: its state lives in the backend, not in the window.
//!
//! The user switches to SillyTavern or closes settings — the download keeps
//! going, and a reopened settings screen shows the same queue: what is
//! downloading, how much is left, and whether it is paused. Pause stops the
//! transport without deleting downloaded data: resuming continues the file from the break point.
//!
//! Installing the runtime, SillyTavern, and ComfyUI is not part of this — it has its own
//! cancellable wizard flow.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::Emitter;

/// The transport paused: this is not an error, the download will continue from the same place.
pub const PAUSED: &str = "загрузка на паузе";
/// Stop via the "Cancel" button — a shared wording for all transports.
pub const CANCELLED: &str = "отменено пользователем";

/// The queue snapshot goes to the interface no more often than this: the transport sends
/// progress more often than the window needs, and an event per chunk floods IPC.
const EMIT_INTERVAL: Duration = Duration::from_millis(400);
/// The speed measurement window for transports that don't compute it: ticks arrive
/// more often (per batch of bytes), but steady speed is what's needed.
const MEASURE_INTERVAL: Duration = Duration::from_millis(500);
/// How often unpausing is checked.
const RESUME_POLL: Duration = Duration::from_millis(150);
/// How often a command waiting for its items reconciles their outcome: the end of an individual
/// item is not signaled, only the end of the whole queue is.
const OUTCOME_POLL: Duration = Duration::from_millis(250);
/// The outcome of items still in progress after the runner's emergency exit.
const INTERRUPTED: &str = "загрузка прервалась внутренней ошибкой — повторите";

pub type FlagsRef = Arc<Flags>;

/// Download progress: received, total, status in words, speed (bytes/s)
/// and the remaining time in seconds. A transport that doesn't compute speed returns zeros —
/// the queue will finish computing it.
pub type ProgressFn = Arc<dyn Fn(u64, u64, String, u64, u64) + Send + Sync>;

/// Download flags visible to the transport.
pub struct Flags {
    /// Stop signal. Shared: wizard cancellation sets the same flag seen by
    /// the runtime, SillyTavern, and ComfyUI.
    cancel: Arc<AtomicBool>,
    paused: AtomicBool,
}

impl Flags {
    pub fn new() -> FlagsRef {
        Arc::new(Self {
            cancel: Arc::new(AtomicBool::new(false)),
            paused: AtomicBool::new(false),
        })
    }

    /// Flags on top of someone else's stop signal (wizard cancellation, ComfyUI installation).
    pub fn with_cancel(cancel: Arc<AtomicBool>) -> FlagsRef {
        Arc::new(Self {
            cancel,
            paused: AtomicBool::new(false),
        })
    }

    /// Whether the transport should stop right now, and why. Pause is checked
    /// first: cancel during a pause is a cancel, not a resume.
    pub fn stop(&self) -> Option<&'static str> {
        if self.paused.load(Ordering::Relaxed) {
            Some(PAUSED)
        } else if self.cancel.load(Ordering::Relaxed) {
            Some(CANCELLED)
        } else {
            None
        }
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
    }

    fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
    }

    fn clear(&self) {
        self.cancel.store(false, Ordering::Relaxed);
        self.paused.store(false, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Ollama,
    Image,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Queued,
    Downloading,
    Paused,
    Done,
    Failed,
    Cancelled,
}

#[derive(Clone, Serialize)]
pub struct Task {
    pub kind: Kind,
    /// Ollama model tag or checkpoint file name.
    pub id: String,
    /// Human-readable name for the queue row.
    pub title: String,
    pub status: Status,
    pub completed: u64,
    pub total: u64,
    pub speed: u64,
    pub eta: u64,
    /// Current status in words from the transport ("pulling sha256-…", "preparing…").
    pub note: String,
    pub error: Option<String>,
    /// Timestamp of the last speed measurement: the queue computes speed from it where
    /// the transport only sends bytes (image checkpoints).
    #[serde(skip)]
    measured: Option<(Instant, u64)>,
}

impl Task {
    pub fn ollama(tag: &str, title: String) -> Self {
        Self {
            kind: Kind::Ollama,
            id: tag.to_string(),
            title,
            status: Status::Queued,
            completed: 0,
            total: 0,
            speed: 0,
            eta: 0,
            note: String::new(),
            error: None,
            measured: None,
        }
    }

    pub fn image(filename: &str, title: String) -> Self {
        Self {
            kind: Kind::Image,
            ..Self::ollama(filename, title)
        }
    }
}

/// Queue snapshot for the interface: what is in it, what is downloading, and whether it is paused.
#[derive(Clone, Default, Serialize)]
pub struct Snapshot {
    pub tasks: Vec<Task>,
    pub paused: bool,
    pub running: bool,
}

/// Queue state without side effects: the wrapper sends events, so
/// transitions are tested directly.
#[derive(Default)]
struct Inner {
    snapshot: Snapshot,
    /// How items ended. Needed by commands that waited for a download: while they
    /// were waking up, the queue could have moved on and started a new run, and the window
    /// that started it could overwrite the task list. The key carries the item kind (V-38):
    /// an Ollama tag and a checkpoint name are compared by different rules.
    results: HashMap<(Kind, String), Result<(), String>>,
    /// An item interrupted not by cancellation but yielding the queue to another: the runner
    /// will return it to waiting, and the downloaded data stays on disk.
    deferred: Option<String>,
    last_emit: Option<Instant>,
}

impl Inner {
    fn take_next(&mut self) -> Option<Task> {
        let index = self
            .snapshot
            .tasks
            .iter()
            .position(|task| task.status == Status::Queued)?;
        self.snapshot.tasks[index].status = Status::Downloading;
        Some(self.snapshot.tasks[index].clone())
    }

    fn tick(
        &mut self,
        id: &str,
        completed: u64,
        total: u64,
        note: &str,
        speed: u64,
        eta: u64,
    ) {
        let Some(task) = self.snapshot.tasks.iter_mut().find(|task| task.id == id) else {
            return;
        };
        task.completed = completed;
        task.total = total;
        task.note = note.to_string();

        if speed > 0 {
            // The transport computes speed itself (Ollama stream).
            task.speed = speed;
            task.eta = eta;
            task.measured = None;
            return;
        }
        match task.measured {
            // The counter went backwards — a new layer started: this is the reference point.
            Some((_, was)) if completed < was => {
                task.measured = Some((Instant::now(), completed));
            }
            Some((at, was)) if at.elapsed() >= MEASURE_INTERVAL => {
                let seconds = at.elapsed().as_secs_f64();
                let speed = ((completed - was) as f64 / seconds) as u64;
                task.speed = speed;
                task.eta = if speed > 0 && total > completed {
                    (total - completed) / speed
                } else {
                    0
                };
                task.measured = Some((Instant::now(), completed));
            }
            // The measurement hasn't filled the window yet: keep the previous numbers as is.
            Some(_) => {}
            None => task.measured = Some((Instant::now(), completed)),
        }
    }

    fn set_status(&mut self, id: &str, status: Status, error: Option<String>) {
        let Some(task) = self.snapshot.tasks.iter_mut().find(|task| task.id == id) else {
            return;
        };
        let kind = task.kind;
        task.status = status;
        task.error = error.clone();
        // A deferred item that ended on its own (success, failure, cancellation) no longer
        // yields the queue: a dangling mark would turn the next cancellation of
        // this item into a re-enqueue.
        if matches!(status, Status::Done | Status::Failed | Status::Cancelled)
            && self.deferred.as_deref() == Some(id)
        {
            self.deferred = None;
        }
        match status {
            Status::Done => {
                self.results.insert((kind, id.to_string()), Ok(()));
            }
            Status::Failed => {
                self.results
                    .insert((kind, id.to_string()), Err(error.unwrap_or_default()));
            }
            Status::Cancelled => {
                self.results.insert((kind, id.to_string()), Err(CANCELLED.into()));
            }
            Status::Queued | Status::Downloading | Status::Paused => {}
        }
    }

    /// Close items the queue hasn't reached yet. The decision "whether to start
    /// an item" is made by its status, not by a global flag: otherwise
    /// a download started by the user right after cancellation would fall under it.
    fn cancel_pending(&mut self) {
        let ids: Vec<String> = self
            .snapshot
            .tasks
            .iter()
            .filter(|task| task.status == Status::Queued)
            .map(|task| task.id.clone())
            .collect();
        for id in ids {
            self.set_status(&id, Status::Cancelled, None);
        }
    }

    /// Put an item next: the queue is sequential, and "resume"
    /// on a waiting row means "download this one, not the one started first".
    fn move_to_front(&mut self, id: &str) {
        let Some(index) = self.snapshot.tasks.iter().position(|task| task.id == id) else {
            return;
        };
        let first_queued = self
            .snapshot
            .tasks
            .iter()
            .position(|task| task.status == Status::Queued)
            .unwrap_or(index);
        if index > first_queued {
            let task = self.snapshot.tasks.remove(index);
            self.snapshot.tasks.insert(first_queued, task);
        }
    }

    /// Send an item to the end of the queue: one that yielded the queue must not
    /// end up first again — the runner takes the first waiting item in list order.
    fn move_to_back(&mut self, id: &str) {
        let Some(index) = self.snapshot.tasks.iter().position(|task| task.id == id) else {
            return;
        };
        let task = self.snapshot.tasks.remove(index);
        self.snapshot.tasks.push(task);
    }

    /// The item whose transport is currently running: downloading or paused.
    /// A waiting item is not included — its transport is not started, and pause or cancel
    /// for it must not touch the shared stop signal.
    fn active(&self) -> Option<&Task> {
        self.snapshot
            .tasks
            .iter()
            .find(|task| matches!(task.status, Status::Downloading | Status::Paused))
    }

    fn is_queued(&self, id: &str) -> bool {
        self.snapshot
            .tasks
            .iter()
            .any(|task| task.id == id && task.status == Status::Queued)
    }

    /// Forget the past outcome of an item under any name form: otherwise a new run
    /// under a different tag form would answer with the old result. Search only among
    /// outcomes of the same kind — name rules are kind-specific (V-38).
    fn forget_result(&mut self, kind: Kind, id: &str) {
        self.results
            .retain(|(key_kind, key), _| !(*key_kind == kind && same_name(kind, key, id)));
    }

    /// Stored outcome of a finished item, looked up by a bare id (the caller
    /// knows no kind). Keys are matched with the rule of their own kind (V-38),
    /// and the pick is deterministic: an exact id wins, then Ollama before
    /// Image, then the smaller key.
    fn find_result(&self, id: &str) -> Option<&Result<(), String>> {
        self.results
            .iter()
            .filter(|((kind, key), _)| same_name(*kind, key, id))
            .min_by_key(|((kind, key), _)| (key != id, kind_rank(*kind), (*key).clone()))
            .map(|(_, result)| result)
    }

    /// Whether the snapshot is ready to be sent: progress arrives more often than the window needs.
    fn ready_to_emit(&mut self, force: bool) -> Option<Snapshot> {
        if !force {
            let fresh = self
                .last_emit
                .is_some_and(|at| at.elapsed() < EMIT_INTERVAL);
            if fresh {
                return None;
            }
        }
        self.last_emit = Some(Instant::now());
        Some(self.snapshot.clone())
    }

    /// Add tasks without duplicates: an item already queued or
    /// downloading is not added twice (otherwise progress would go to one row
    /// and the second would hang forever), and a finished one is replaced by a new run.
    /// A new run goes to the end like any new task: in place of the old
    /// row it would end up ahead of the running download, and the "first active
    /// row" (ST panel) would show a waiting item instead of a downloading one.
    fn merge(&mut self, tasks: Vec<Task>) {
        for task in tasks {
            let index = self
                .snapshot
                .tasks
                .iter()
                .position(|existing| same_item(existing, task.kind, &task.id));
            match index {
                Some(index) if is_active(self.snapshot.tasks[index].status) => {}
                Some(index) => {
                    self.snapshot.tasks.remove(index);
                    self.forget_result(task.kind, &task.id);
                    self.snapshot.tasks.push(task);
                }
                None => {
                    self.forget_result(task.kind, &task.id);
                    self.snapshot.tasks.push(task);
                }
            }
        }
    }
}

fn is_active(status: Status) -> bool {
    matches!(
        status,
        Status::Queued | Status::Downloading | Status::Paused
    )
}

/// The same queue item: the kind must match, and the name is compared by
/// this kind's rules. Ollama model tag forms differ (`bge-m3` and
/// `bge-m3:latest`, with the `library/` prefix), so we compare them by the same
/// rule as installed models: otherwise one download would occupy two
/// rows and both would show its progress. An image checkpoint name is exact.
fn same_item(existing: &Task, kind: Kind, id: &str) -> bool {
    existing.kind == kind && same_name(kind, &existing.id, id)
}

/// One name within one kind (V-38). Ollama tags have several spellings of the
/// same item (`x`, `x:latest`, `library/x`) and are matched by the installed-
/// models rule; a checkpoint filename is matched exactly (case-insensitive) —
/// tag rules must not leak across id namespaces.
fn same_name(kind: Kind, a: &str, b: &str) -> bool {
    match kind {
        Kind::Ollama => crate::models::is_model_installed(&[a.to_string()], b),
        Kind::Image => a.eq_ignore_ascii_case(b),
    }
}

/// Deterministic tie-break order for outcome lookups across kinds.
fn kind_rank(kind: Kind) -> u8 {
    match kind {
        Kind::Ollama => 0,
        Kind::Image => 1,
    }
}

/// Download queue. Lives in application state: any window sees it.
pub struct Queue {
    inner: Mutex<Inner>,
    flags: FlagsRef,
    running: tokio::sync::watch::Sender<bool>,
    /// The receiver we keep for ourselves: for a channel without subscribers `send` does not
    /// update the value, and the queue would always consider itself free.
    _state: tokio::sync::watch::Receiver<bool>,
}

impl Queue {
    pub fn new() -> Arc<Self> {
        let (running, state) = tokio::sync::watch::channel(false);
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            flags: Flags::new(),
            running,
            _state: state,
        })
    }

    pub fn snapshot(&self) -> Snapshot {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).snapshot.clone()
    }

    /// How the queue item ended. `None` — the item is still in progress or never started:
    /// by this answer, the command waiting for the download returns its result to the interface.
    /// The result survives the queue's next run — the command reads it already
    /// after waking up, and the window could have started a new download at that moment.
    pub fn outcome(&self, id: &str) -> Option<Result<(), String>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // The item name could arrive in a different form (`bge-m3` instead of `bge-m3:latest`):
        // it is the same item, and its result is the same.
        let task = inner
            .snapshot
            .tasks
            .iter()
            .find(|task| same_item(task, task.kind, id));
        match task.map(|task| (task.status, task.error.clone())) {
            Some((Status::Done, _)) => Some(Ok(())),
            Some((Status::Failed, error)) => Some(Err(error.unwrap_or_default())),
            Some((Status::Cancelled, _)) => Some(Err(CANCELLED.into())),
            Some(_) => None,
            None => inner.find_result(id).cloned(),
        }
    }

    /// Enqueue tasks. `true` — the queue was idle and needs to be started:
    /// a busy queue simply accepts tasks at the end.
    ///
    /// The "running" state changes under the same lock as the task list: otherwise
    /// a second window pressing "Download" at the same moment would overwrite the other's
    /// queue, and a task added at the very finish would be left without a runner.
    pub fn enqueue(&self, tasks: Vec<Task>) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.snapshot.running {
            inner.merge(tasks);
            return false;
        }
        self.flags.clear();
        inner.last_emit = None;
        inner.snapshot.tasks.clear();
        inner.snapshot.paused = false;
        inner.deferred = None;
        inner.merge(tasks);
        // The running flag lives in the snapshot itself: by it the interface shows
        // pause and cancel, and the event carries the whole snapshot. The watch channel is only
        // a signal to waiting commands.
        inner.snapshot.running = true;
        let _ = self.running.send(true);
        true
    }

    /// Wait until its own items finish: a command that enqueued a download answers
    /// by them, not by the end of the whole queue — other downloads may be behind
    /// (background vision, the model menu in ST). An emptied queue also releases:
    /// a command will return an item without an outcome as unfinished.
    pub async fn wait_for(&self, ids: &[String]) {
        // A deadline is mandatory: a paused queue stays running, and a command
        // that enqueued a download (LoRA export) would wait forever without a single
        // explanation. On timeout the command returns "not completed".
        const WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(3 * 3600);
        let deadline = tokio::time::Instant::now() + WAIT_LIMIT;
        let mut rx = self.running.subscribe();
        loop {
            let running = *rx.borrow_and_update();
            if !running || ids.iter().all(|id| self.outcome(id).is_some()) {
                return;
            }
            // Pause is not work: the command waiting for the download would otherwise hang until
            // "Resume" or the three-hour deadline and only then say
            // "not completed", even though the item is still queued.
            if self.snapshot().paused {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            let _ = tokio::time::timeout(OUTCOME_POLL, rx.changed()).await;
        }
    }

    pub fn pause(&self, app: &tauri::AppHandle) {
        self.flags.pause();
        self.change(app, |inner| inner.snapshot.paused = true);
    }

    pub fn resume(&self, app: &tauri::AppHandle) {
        self.flags.resume();
        self.change(app, |inner| inner.snapshot.paused = false);
    }

    /// Pause of a single item. Only one downloads at a time, so pausing the active
    /// item pauses the queue; there is nothing to pause on a waiting one.
    pub fn pause_task(&self, app: &tauri::AppHandle, id: &str) {
        if self.pause_task_inner(id) {
            self.emit(app, true);
        }
    }

    /// Row button flags are set under the same lock under which the
    /// active item was found: `take_next_inner` takes the next under the same lock, and a signal
    /// set after releasing the lock could go to a different item already.
    fn pause_task_inner(&self, id: &str) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.active().map(|task| task.id.as_str()) != Some(id) {
            return false;
        }
        self.flags.pause();
        inner.snapshot.paused = true;
        true
    }

    /// Resume a specific item. For the active one this unpauses it, and for
    /// a waiting one it means "download this": the item goes first, and the one that ran before
    /// returns to the queue (downloaded data stays on disk and will be appended later).
    /// Previously both buttons called one command for the whole queue, and "Resume" on
    /// any row continued the download that started first.
    pub fn resume_task(&self, app: &tauri::AppHandle, id: &str) {
        self.resume_task_inner(id);
        self.emit(app, true);
    }

    fn resume_task_inner(&self, id: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let active = inner.active().map(|task| task.id.clone());
        match plan_resume(active.as_deref(), id) {
            ResumeAction::ResumeQueue => {
                self.flags.resume();
                inner.snapshot.paused = false;
            }
            // Yielding the queue makes sense only for a waiting item: otherwise the current
            // download would be interrupted and immediately started again.
            ResumeAction::Defer(_) if !inner.is_queued(id) => {}
            ResumeAction::Defer(active_id) => {
                inner.move_to_front(id);
                inner.deferred = Some(active_id);
                inner.snapshot.paused = false;
                // Interrupt the current run: the runner will see the mark and return
                // the interrupted item to waiting, and take the chosen one next.
                self.flags.cancel_now();
            }
            ResumeAction::StartQueued => {
                inner.move_to_front(id);
                self.flags.resume();
                inner.snapshot.paused = false;
            }
        }
    }

    /// Cancel one item: the active one is aborted, a waiting one is simply closed.
    /// The queue keeps working — we don't touch the remaining items.
    pub fn cancel_task(&self, app: &tauri::AppHandle, id: &str) {
        self.cancel_task_inner(id);
        self.emit(app, true);
    }

    fn cancel_task_inner(&self, id: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.active().map(|task| task.id.as_str()) == Some(id) {
            inner.snapshot.paused = false;
            inner.deferred = None;
            self.flags.cancel_now();
        } else if inner.is_queued(id) {
            // Don't rewrite a finished item: its outcome was already read
            // by the command waiting for the download, or will be read.
            inner.set_status(id, Status::Cancelled, None);
        }
    }

    /// Cancel: the current download is aborted, remaining items are closed.
    /// Downloaded data stays on disk — the next attempt will continue from that point.
    pub fn cancel(&self, app: &tauri::AppHandle) {
        self.cancel_inner();
        self.emit(app, true);
    }

    fn cancel_inner(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.snapshot.paused = false;
        // Global cancel takes precedence over "download this": the yielded item is closed, not
        // returned to the queue and not started again.
        inner.deferred = None;
        // The stop signal is seen only by the current transport and items already queued:
        // a download started by the user right after cancellation is not covered by it.
        self.flags.cancel_now();
        inner.cancel_pending();
    }

    /// The item was interrupted for another one (not cancelled): return it to waiting and
    /// send it to the end of the queue — otherwise the runner will take it first again, and
    /// "Download this" on a neighboring row would change nothing.
    fn requeue_deferred(&self, app: &tauri::AppHandle, id: &str) -> bool {
        let deferred = self.requeue_deferred_inner(id);
        if deferred {
            self.emit(app, true);
        }
        deferred
    }

    fn requeue_deferred_inner(&self, id: &str) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.deferred.as_deref() != Some(id) {
            return false;
        }
        inner.deferred = None;
        inner.set_status(id, Status::Queued, None);
        inner.move_to_back(id);
        true
    }

    /// Publish the snapshot: a task could be added to a busy queue, and there are no progress
    /// ticks while paused — without this the window won't know about it.
    pub fn publish(&self, app: &tauri::AppHandle) {
        self.emit(app, true);
    }

    /// Progress tick from the transport.
    pub fn tick(
        &self,
        app: &tauri::AppHandle,
        id: &str,
        completed: u64,
        total: u64,
        note: &str,
        speed: u64,
        eta: u64,
    ) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .tick(id, completed, total, note, speed, eta);
        self.emit(app, false);
    }

    pub fn set_status(
        &self,
        app: &tauri::AppHandle,
        id: &str,
        status: Status,
        error: Option<String>,
    ) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).set_status(id, status, error);
        self.emit(app, true);
    }

    fn take_next(&self, app: &tauri::AppHandle) -> Option<Task> {
        let task = self.take_next_inner();
        self.emit(app, true);
        task
    }

    fn take_next_inner(&self) -> Option<Task> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let task = inner.take_next();
        if task.is_some() {
            // The item is taken with a clean stop signal: a cancel pressed
            // earlier has already closed all queued items (see `cancel_inner`), and this one
            // the user enqueued after it. The flag is cleared under the same
            // lock it is set with cancellation — otherwise they would diverge.
            self.flags.clear_cancel();
        }
        task
    }

    /// Finish work. `true` — the queue is really empty; `false` — a task
    /// managed to get in, and the runner must continue the loop.
    fn finish(&self, app: &tauri::AppHandle) -> bool {
        let done = self.finish_inner();
        self.emit(app, true);
        done
    }

    fn finish_inner(&self) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner
            .snapshot
            .tasks
            .iter()
            .any(|task| task.status == Status::Queued)
        {
            return false;
        }
        inner.snapshot.paused = false;
        inner.snapshot.running = false;
        self.flags.resume();
        let _ = self.running.send(false);
        true
    }

    /// Clear the running flag without asking the queue: the runner's emergency exit.
    /// A panic in the transport would otherwise leave the queue "running" forever, and
    /// commands waiting for it would hang without an answer.
    fn force_finish(&self, app: &tauri::AppHandle) {
        let snapshot = self.force_finish_inner();
        let _ = app.emit("downloads-changed", snapshot);
    }

    /// The same cleanup path without emit: tests run it where AppHandle is unavailable.
    fn force_finish_inner(&self) -> Snapshot {
        let snapshot = {
            // A poisoned lock is also readable: a transport panic is exactly what poisons
            // it, and running.send(false) must happen in any case.
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            // The runner is gone: rows "downloading/waiting" would remain with buttons
            // that start nothing, and waiting commands without an outcome.
            let leftover: Vec<String> = inner
                .snapshot
                .tasks
                .iter()
                .filter(|task| is_active(task.status))
                .map(|task| task.id.clone())
                .collect();
            for id in leftover {
                inner.set_status(&id, Status::Failed, Some(INTERRUPTED.into()));
            }
            inner.deferred = None;
            inner.snapshot.paused = false;
            inner.snapshot.running = false;
            inner.snapshot.clone()
        };
        self.flags.resume();
        let _ = self.running.send(false);
        snapshot
    }

    fn change(&self, app: &tauri::AppHandle, change: impl FnOnce(&mut Inner)) {
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            change(&mut inner);
        }
        self.emit(app, true);
    }

    fn emit(&self, app: &tauri::AppHandle, force: bool) {
        let snapshot = self.inner.lock().unwrap_or_else(|e| e.into_inner()).ready_to_emit(force);
        if let Some(snapshot) = snapshot {
            let _ = app.emit("downloads-changed", snapshot);
        }
    }
}

impl Flags {
    /// Cancellation during a pause: the pause flag is cleared, otherwise the transport would keep
    /// waiting for resume instead of stopping.
    fn cancel_now(&self) {
        self.paused.store(false, Ordering::Relaxed);
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Clear the stop signal before a new task (see `Queue::take_next_inner`).
    fn clear_cancel(&self) {
        self.cancel.store(false, Ordering::Relaxed);
    }
}

/// What to do on pressing "Resume" on a queue row.
#[derive(Debug, PartialEq, Eq)]
enum ResumeAction {
    /// The row is the current download — unpause it.
    ResumeQueue,
    /// Another one is requested: yield the current (its id is inside), put the chosen one first.
    Defer(String),
    /// There is no active download — just start with the chosen one.
    StartQueued,
}

fn plan_resume(active: Option<&str>, id: &str) -> ResumeAction {
    match active {
        Some(active_id) if active_id == id => ResumeAction::ResumeQueue,
        Some(active_id) => ResumeAction::Defer(active_id.to_string()),
        None => ResumeAction::StartQueued,
    }
}

/// Wait for unpause. Pause is a user action, not a data stream:
/// frequent polling is cheaper here than a separate signal channel.
async fn wait_resume(flags: &Flags) {
    while flags.is_paused() && !flags.cancelled() {
        tokio::time::sleep(RESUME_POLL).await;
    }
}

fn stopped_by_pause(outcome: &Result<(), String>) -> bool {
    outcome.as_ref().err().is_some_and(|error| error == PAUSED)
}

/// Throttle progress events outside the queue (wizard, installers): the transport
/// calls the callback on every chunk, and an event per chunk means tens of thousands of
/// IPC messages per gigabyte. State changes and download end always pass through.
pub struct ProgressThrottle {
    last: Option<Instant>,
    status: String,
}

impl ProgressThrottle {
    pub fn new() -> Self {
        Self {
            last: None,
            status: String::new(),
        }
    }

    pub fn due(&mut self, completed: u64, total: u64, status: &str) -> bool {
        let finished = total > 0 && completed >= total;
        let due = finished
            || self.status != status
            || self.last.is_none_or(|at| at.elapsed() >= EMIT_INTERVAL);
        if due {
            self.last = Some(Instant::now());
            if self.status != status {
                self.status = status.to_string();
            }
        }
        due
    }
}

/// The runner clears the running flag also on emergency exit (panic in the transport):
/// otherwise the queue would stay "running" and commands waiting for it without an answer.
struct FinishOnDrop(Option<(Arc<Queue>, tauri::AppHandle)>);

impl FinishOnDrop {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for FinishOnDrop {
    fn drop(&mut self) {
        if let Some((queue, app)) = &self.0 {
            queue.force_finish(app);
        }
    }
}

/// Play the queue: each item is given to the transport, a pause waits for resume
/// and starts it again — the transport continues the file from the break point.
pub async fn run<F, Fut>(queue: Arc<Queue>, app: tauri::AppHandle, run_item: F)
where
    F: Fn(Task, FlagsRef) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>> + Send,
{
    let mut guard = FinishOnDrop(Some((queue.clone(), app.clone())));
    loop {
        while let Some(task) = queue.take_next(&app) {
            let mut outcome = run_item(task.clone(), queue.flags.clone()).await;
            while stopped_by_pause(&outcome) {
                queue.set_status(&app, &task.id, Status::Paused, None);
                wait_resume(&queue.flags).await;
                if queue.flags.cancelled() {
                    break;
                }
                queue.set_status(&app, &task.id, Status::Downloading, None);
                outcome = run_item(task.clone(), queue.flags.clone()).await;
            }
            match outcome {
                Ok(()) => queue.set_status(&app, &task.id, Status::Done, None),
                // A pause here is a cancel that caught up with the download during the pause.
                Err(error) if error == CANCELLED || error == PAUSED => {
                    if !queue.requeue_deferred(&app, &task.id) {
                        queue.set_status(&app, &task.id, Status::Cancelled, None);
                    }
                }
                Err(error) => queue.set_status(&app, &task.id, Status::Failed, Some(error)),
            }
        }
        if queue.finish(&app) {
            break;
        }
    }
    guard.disarm();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str) -> Task {
        Task::ollama(id, format!("модель {id}"))
    }

    #[test]
    fn test_pause_stops_transport_before_cancel() {
        let flags = Flags::new();
        assert_eq!(flags.stop(), None);
        flags.pause();
        assert_eq!(flags.stop(), Some(PAUSED));
        flags.cancel_now();
        assert_eq!(flags.stop(), Some(CANCELLED), "отмена важнее паузы");
    }

    #[test]
    fn test_foreign_stop_signal_shared_with_master_cancel() {
        // Flags on top of someone else's cancellation (wizard): the stop is visible to both.
        let cancel = Arc::new(AtomicBool::new(false));
        let flags = Flags::with_cancel(cancel.clone());
        assert_eq!(flags.stop(), None);
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(flags.stop(), Some(CANCELLED));
    }

    #[test]
    fn test_take_next_goes_in_order_and_marks_downloading() {
        let mut inner = Inner::default();
        inner.snapshot.tasks = vec![task("a"), task("b")];
        let first = inner.take_next().unwrap();
        assert_eq!(first.id, "a");
        assert_eq!(first.status, Status::Downloading);
        assert_eq!(inner.take_next().unwrap().id, "b");
        assert!(inner.take_next().is_none());
    }

    #[test]
    fn test_speed_is_measured_when_transport_does_not_report_it() {
        let mut inner = Inner::default();
        inner.snapshot.tasks = vec![task("a")];
        inner.tick("a", 1_000_000, 40_000_000, "качаю…", 0, 0);
        assert_eq!(inner.snapshot.tasks[0].speed, 0, "на первом тике мерить нечем");
        std::thread::sleep(MEASURE_INTERVAL + Duration::from_millis(200));
        inner.tick("a", 2_000_000, 40_000_000, "качаю…", 0, 0);
        let measured = &inner.snapshot.tasks[0];
        assert!(measured.speed > 0, "скорость считается по дельте байт");
        assert!(measured.eta > 0, "остаток времени считается из скорости");
    }

    #[test]
    fn test_speed_survives_frequent_ticks() {
        let mut inner = Inner::default();
        inner.snapshot.tasks = vec![task("a")];
        // The checkpoint transport sends a tick per batch of bytes: the measurement window
        // must survive frequent calls, otherwise speed remains zero.
        for step in 1..=20 {
            inner.tick("a", step * 100_000, 40_000_000, "качаю…", 0, 0);
            std::thread::sleep(Duration::from_millis(40));
        }
        let task = &inner.snapshot.tasks[0];
        assert!(task.speed > 0, "частые тики не дали измерить скорость");
        assert!(task.eta > 0, "остаток времени считается из скорости");
    }

    #[test]
    fn test_transport_speed_wins_over_measurement() {
        let mut inner = Inner::default();
        inner.snapshot.tasks = vec![task("a")];
        inner.tick("a", 1_000_000, 4_000_000, "качаю…", 500_000, 6);
        let task = &inner.snapshot.tasks[0];
        assert_eq!(task.speed, 500_000);
        assert_eq!(task.eta, 6);
    }

    #[test]
    fn test_outcome_survives_the_next_run() {
        let queue = Queue::new();
        assert!(queue.enqueue(vec![task("a")]));
        queue
            .inner
            .lock()
            .unwrap()
            .set_status("a", Status::Done, None);
        assert!(queue.finish_inner());
        assert_eq!(queue.outcome("a"), Some(Ok(())));

        // Another window started a new run and overwrote the task list: the result
        // of the past item must remain — the command waiting for the download reads
        // it after waking up already.
        assert!(queue.enqueue(vec![task("b")]));
        assert_eq!(
            queue.outcome("a"),
            Some(Ok(())),
            "результат прошлого пункта потерян вместе со снимком"
        );
        // And a new run of the same item starts from a clean slate.
        queue
            .inner
            .lock()
            .unwrap()
            .set_status("b", Status::Failed, Some("нет сети".into()));
        assert_eq!(queue.outcome("b"), Some(Err("нет сети".into())));
        assert!(!queue.enqueue(vec![task("b")]), "очередь занята — встаём в конец");
        assert_eq!(queue.outcome("b"), None, "новый заход сбрасывает прошлый результат");
    }

    #[test]
    fn test_unfinished_item_has_no_outcome() {
        let queue = Queue::new();
        assert!(queue.enqueue(vec![task("a")]));
        assert_eq!(queue.outcome("a"), None, "пункт ещё не начинался");
        queue.take_next_inner();
        assert_eq!(queue.outcome("a"), None, "пункт качается");
        queue
            .inner
            .lock()
            .unwrap()
            .set_status("a", Status::Cancelled, None);
        assert_eq!(queue.outcome("a"), Some(Err(CANCELLED.into())));
    }

    #[test]
    fn test_progress_emits_are_throttled_but_status_changes_are_not() {
        let mut inner = Inner::default();
        inner.snapshot.tasks = vec![task("a")];
        assert!(inner.ready_to_emit(true).is_some());
        assert!(inner.ready_to_emit(false).is_none(), "частый прогресс не шлём");
        assert!(inner.ready_to_emit(true).is_some(), "смена состояния идёт сразу");
        std::thread::sleep(EMIT_INTERVAL + Duration::from_millis(50));
        assert!(inner.ready_to_emit(false).is_some());
    }

    #[test]
    fn test_snapshot_carries_the_running_flag() {
        // The running flag goes to the interface inside the snapshot (both by command and
        // by event): by it the settings screen shows pause and cancel.
        let queue = Queue::new();
        assert!(!queue.snapshot().running);
        queue.enqueue(vec![task("a")]);
        assert!(queue.snapshot().running, "снимок не сказал, что очередь идёт");
        queue
            .inner
            .lock()
            .unwrap()
            .set_status("a", Status::Paused, None);
        assert!(queue.snapshot().running, "пауза — это всё ещё идущая очередь");
        assert!(queue.finish_inner());
        assert!(!queue.snapshot().running);
    }

    #[test]
    fn test_pause_and_resume_cycles_keep_the_item() {
        let queue = Queue::new();
        queue.enqueue(vec![task("a")]);
        assert_eq!(queue.take_next_inner().unwrap().status, Status::Downloading);
        // Pause and resume can alternate as much as needed: the item remains
        // itself, and it has no result until the download finishes.
        for _ in 0..3 {
            queue
                .inner
                .lock()
                .unwrap()
                .set_status("a", Status::Paused, None);
            assert!(
                !queue.snapshot().paused,
                "пауза пункта не равна паузе всей очереди"
            );
            assert_eq!(queue.outcome("a"), None, "на паузе результата ещё нет");
            queue
                .inner
                .lock()
                .unwrap()
                .set_status("a", Status::Downloading, None);
        }
        queue
            .inner
            .lock()
            .unwrap()
            .set_status("a", Status::Done, None);
        assert_eq!(queue.outcome("a"), Some(Ok(())));
    }

    #[test]
    fn test_cancel_closes_pending_items_in_any_state_of_the_current_one() {
        // The current item is closed by the transport (it sees the stop signal), and
        // those behind it are closed by the cancellation itself — regardless of whether
        // the current one is downloading, paused, or has just been taken.
        for status in [Status::Queued, Status::Downloading, Status::Paused] {
            let queue = Queue::new();
            queue.enqueue(vec![task("a"), task("b")]);
            {
                let mut inner = queue.inner.lock().unwrap();
                if status != Status::Queued {
                    inner.take_next();
                }
                inner.set_status("a", status, None);
            }

            queue.cancel_inner();
            assert!(
                queue.flags.cancelled(),
                "сигнал остановки не выставлен (текущий: {status:?})"
            );
            assert_eq!(
                queue.outcome("b"),
                Some(Err(CANCELLED.into())),
                "стоящий пункт не закрыт (текущий: {status:?})"
            );
        }
    }

    #[test]
    fn test_mixed_kinds_live_in_one_queue() {
        let queue = Queue::new();
        queue.enqueue(vec![
            Task::ollama("bge-m3", "память".into()),
            Task::image("AnythingV5.safetensors", "Anything V5".into()),
        ]);
        assert_eq!(queue.take_next_inner().unwrap().kind, Kind::Ollama);
        assert_eq!(queue.take_next_inner().unwrap().kind, Kind::Image);
        assert!(queue.take_next_inner().is_none());
        // Identical names for different kinds are different items.
        assert!(!queue.enqueue(vec![Task::image("bge-m3", "не модель Ollama".into())]));
        assert_eq!(queue.snapshot().tasks.len(), 3);
    }

    #[test]
    fn test_second_run_does_not_inherit_pause_or_results() {
        let queue = Queue::new();
        queue.enqueue(vec![task("a")]);
        queue
            .inner
            .lock()
            .unwrap()
            .set_status("a", Status::Failed, Some("нет сети".into()));
        assert!(queue.finish_inner());
        assert_eq!(queue.outcome("a"), Some(Err("нет сети".into())));

        // A new run: the past pause and past results do not affect it.
        queue.flags.pause();
        assert!(queue.enqueue(vec![task("b")]));
        assert!(!queue.snapshot().paused, "новая очередь не начинается на паузе");
        assert_eq!(queue.outcome("b"), None);
        assert_eq!(queue.flags.stop(), None, "сигнал остановки снят новой очередью");
    }

    #[test]
    fn test_queue_keeps_one_row_per_item() {
        let queue = Queue::new();
        assert!(queue.enqueue(vec![task("a")]));
        // The same item does not appear twice: its row is already in the queue.
        assert!(!queue.enqueue(vec![task("a"), task("b")]));
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.tasks.len(), 2, "получено: {:?}", snapshot.tasks.len());

        // A finished item is replaced by a new run with clean progress.
        queue
            .inner
            .lock()
            .unwrap()
            .set_status("a", Status::Done, None);
        queue
            .inner
            .lock()
            .unwrap()
            .tick("a", 5_000, 5_000, "готово", 0, 0);
        assert!(!queue.enqueue(vec![task("a")]));
        let retried = queue
            .snapshot()
            .tasks
            .into_iter()
            .find(|task| task.id == "a")
            .unwrap();
        assert_eq!(retried.status, Status::Queued);
        assert_eq!(retried.completed, 0);
    }

    #[test]
    fn test_task_added_at_finish_keeps_runner_alive() {
        let queue = Queue::new();
        assert!(queue.enqueue(vec![task("a")]), "свободная очередь запускает бегунок");
        assert_eq!(queue.take_next_inner().unwrap().id, "a");
        // While the runner is working, a second task goes to the end.
        assert!(!queue.enqueue(vec![task("b")]));
        assert!(
            !queue.finish_inner(),
            "в очереди есть задача — бегунок обязан продолжить, а не погасить её"
        );
        assert_eq!(queue.take_next_inner().unwrap().id, "b");
        assert!(queue.finish_inner());
        // An empty queue is free again: the next task raises a new runner.
        assert!(queue.enqueue(vec![task("c")]));
        assert_eq!(queue.take_next_inner().unwrap().id, "c");
        assert!(queue.finish_inner());
    }

    #[test]
    fn test_cancel_closes_pending_and_spares_the_next_download() {
        let queue = Queue::new();
        queue.enqueue(vec![task("a"), task("b")]);
        assert_eq!(queue.take_next_inner().unwrap().id, "a");

        queue.cancel_inner();
        assert!(queue.flags.cancelled(), "текущий транспорт обязан остановиться");
        assert_eq!(
            queue.outcome("b"),
            Some(Err(CANCELLED.into())),
            "пункт, до которого очередь не дошла, закрыт отменой"
        );

        // The user immediately starts another model: it goes into the same queue
        // and must download — cancellation does not affect it.
        assert!(!queue.enqueue(vec![task("c")]));
        assert_eq!(queue.take_next_inner().unwrap().id, "c");
        assert!(
            !queue.flags.cancelled(),
            "сигнал остановки накрыл загрузку, начатую после отмены"
        );
        assert_eq!(queue.flags.stop(), None);
    }

    #[test]
    fn test_tag_forms_are_one_item() {
        let queue = Queue::new();
        assert!(queue.enqueue(vec![Task::ollama("bge-m3", "память".into())]));
        // The second form of the same tag is the same item, not a second row.
        assert!(!queue.enqueue(vec![Task::ollama("bge-m3:latest", "память".into())]));
        assert_eq!(queue.snapshot().tasks.len(), 1);
        queue
            .inner
            .lock()
            .unwrap()
            .set_status("bge-m3", Status::Done, None);
        assert_eq!(queue.outcome("bge-m3:latest"), Some(Ok(())));
    }

    /// V-38: outcomes of different kinds are searched by their kind's rules. Ollama tag
    /// forms (`x` ↔ `x:latest`) must not apply to checkpoint names, while Ollama
    /// tags keep being found in any form — even after a new queue
    /// run, when the rows have already been overwritten.
    #[test]
    fn test_outcome_kinds_match_with_their_own_rules() {
        let queue = Queue::new();
        assert!(queue.enqueue(vec![Task::image("model:latest", "чекпоинт".into())]));
        queue
            .inner
            .lock()
            .unwrap()
            .set_status("model:latest", Status::Done, None);
        assert!(queue.finish_inner());

        assert_eq!(queue.outcome("model:latest"), Some(Ok(())));
        assert_eq!(
            queue.outcome("MODEL:latest"),
            Some(Ok(())),
            "имя чекпоинта сравнивается без учёта регистра"
        );
        assert_eq!(
            queue.outcome("model"),
            None,
            "правило тегов Ollama к чекпоинтам неприменимо"
        );

        // An Ollama tag is found in any form, even when the row has already been overwritten
        // by a new run and the result lives only in the outcomes.
        assert!(queue.enqueue(vec![Task::ollama("bge-m3:latest", "память".into())]));
        queue
            .inner
            .lock()
            .unwrap()
            .set_status("bge-m3:latest", Status::Done, None);
        assert!(queue.finish_inner());
        assert_eq!(queue.outcome("bge-m3"), Some(Ok(())), "формы тега Ollama — один пункт");
        assert_eq!(queue.outcome("model:latest"), Some(Ok(())), "итог чекпоинта пережил новый заход");
    }

    #[test]
    fn test_waiting_command_sees_the_end_of_the_queue() {
        let queue = Queue::new();
        assert!(queue.enqueue(vec![task("a")]));
        assert_eq!(queue.outcome("a"), None, "пока идёт — результата нет");
        queue
            .inner
            .lock()
            .unwrap()
            .set_status("a", Status::Done, None);
        assert_eq!(queue.outcome("a"), Some(Ok(())));
        assert!(queue.finish_inner());
    }

    /// The "Resume" button on a waiting row must start exactly it.
    /// Previously all buttons called a command for the whole queue, and the download
    /// that started first was continued.
    #[test]
    fn test_resume_on_queued_defers_active_and_moves_it_up() {
        assert_eq!(plan_resume(Some("a"), "a"), ResumeAction::ResumeQueue);
        assert_eq!(plan_resume(Some("a"), "b"), ResumeAction::Defer("a".into()));
        assert_eq!(plan_resume(None, "b"), ResumeAction::StartQueued);

        let mut inner = Inner::default();
        inner.merge(vec![task("a"), task("b"), task("c")]);
        inner.take_next();
        assert_eq!(inner.active().map(|t| t.id.clone()), Some("a".to_string()));

        inner.move_to_front("c");
        let order: Vec<&str> = inner.snapshot.tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(order, vec!["a", "c", "b"], "выбранная встала первой в ожидании");
        assert_eq!(inner.snapshot.tasks[0].status, Status::Downloading, "активная не тронута");
    }

    /// An item that yielded the queue returns to waiting last: if
    /// left in its previous place, the runner would take it first again, and
    /// "Download this" on a neighboring row would change nothing.
    #[test]
    fn test_deferred_task_goes_to_the_end_of_the_queue() {
        let mut inner = Inner::default();
        inner.merge(vec![task("a"), task("b"), task("c")]);
        inner.take_next();
        inner.move_to_front("c");

        inner.set_status("a", Status::Queued, None);
        inner.move_to_back("a");

        let order: Vec<&str> = inner.snapshot.tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(order, vec!["c", "b", "a"]);
        assert_eq!(inner.take_next().unwrap().id, "c", "следующей идёт выбранная");
    }

    /// Cancelling one row closes only it: neighbors remain in the queue.
    #[test]
    fn test_cancel_one_keeps_the_rest_queued() {
        let mut inner = Inner::default();
        inner.merge(vec![task("a"), task("b"), task("c")]);
        inner.take_next();

        inner.set_status("b", Status::Cancelled, None);

        let statuses: Vec<Status> = inner.snapshot.tasks.iter().map(|t| t.status).collect();
        assert_eq!(statuses, vec![Status::Downloading, Status::Cancelled, Status::Queued]);
    }

    /// The runner's emergency exit must release wait_for even with a poisoned
    /// lock: a panic in the transport is exactly what poisons it.
    #[test]
    fn test_force_finish_releases_waiters_despite_poisoned_inner() {
        let queue = Queue::new();
        queue.enqueue(vec![task("a")]);
        assert!(*queue.running.borrow(), "после enqueue очередь считается идущей");

        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = queue.inner.lock().unwrap();
            panic!("падение транспорта под замком");
        }));

        queue.force_finish_inner();
        assert!(!*queue.running.borrow(), "running не сброшен — wait_for зависла бы");
        assert!(!queue.snapshot().running);
    }

    /// Rows left "in progress" after the runner's emergency exit get
    /// an outcome: without it the window would draw them active with dead buttons, and
    /// the command waiting for its item would answer "download not completed".
    #[test]
    fn test_force_finish_closes_rows_left_in_progress() {
        let queue = Queue::new();
        queue.enqueue(vec![task("a"), task("b"), task("c")]);
        queue
            .inner
            .lock()
            .unwrap()
            .set_status("a", Status::Done, None);
        queue.take_next_inner();

        queue.force_finish_inner();

        let statuses: Vec<Status> = queue.snapshot().tasks.iter().map(|t| t.status).collect();
        assert_eq!(statuses, vec![Status::Done, Status::Failed, Status::Failed]);
        assert_eq!(queue.outcome("a"), Some(Ok(())), "готовый пункт не трогаем");
        assert_eq!(queue.outcome("c"), Some(Err(INTERRUPTED.into())));
    }

    /// Re-downloading a finished item with a neighboring one running: rows
    /// [a done, b downloading] + "Download" on a. b's buttons must control b,
    /// not the waiting a that used to be first in the list.
    #[test]
    fn test_redownload_of_finished_row_does_not_steal_the_active_one() {
        let queue = Queue::new();
        queue.enqueue(vec![task("a"), task("b")]);
        queue.take_next_inner();
        queue
            .inner
            .lock()
            .unwrap()
            .set_status("a", Status::Done, None);
        assert_eq!(queue.take_next_inner().unwrap().id, "b");

        assert!(!queue.enqueue(vec![task("a")]), "очередь занята — встаём в конец");
        let order: Vec<(String, Status)> = queue
            .snapshot()
            .tasks
            .into_iter()
            .map(|t| (t.id, t.status))
            .collect();
        assert_eq!(
            order,
            vec![("b".into(), Status::Downloading), ("a".into(), Status::Queued)],
            "новый заход встаёт за идущей загрузкой"
        );
        assert_eq!(
            queue.inner.lock().unwrap().active().map(|t| t.id.clone()).as_deref(),
            Some("b")
        );

        assert!(queue.pause_task_inner("b"), "пауза b дошла до транспорта");
        assert_eq!(queue.flags.stop(), Some(PAUSED));
        queue.resume_task_inner("b");
        assert_eq!(queue.flags.stop(), None);

        queue.cancel_task_inner("a");
        assert!(!queue.flags.cancelled(), "отмена ждущей a оборвала бы b");
        assert_eq!(queue.outcome("a"), Some(Err(CANCELLED.into())));

        queue.cancel_task_inner("b");
        assert!(queue.flags.cancelled(), "отмена b обязана остановить её транспорт");
    }

    /// While the runner is between items (none is downloading), "Download this" simply
    /// puts the row first: there is nothing to yield, and no "deferred" mark is needed.
    #[test]
    fn test_resume_on_queued_without_running_transport_starts_it() {
        let queue = Queue::new();
        queue.enqueue(vec![task("a"), task("b")]);

        queue.resume_task_inner("b");

        assert!(queue.inner.lock().unwrap().deferred.is_none());
        assert!(!queue.flags.cancelled());
        assert_eq!(queue.take_next_inner().unwrap().id, "b");
    }

    /// Global cancel after "Download this": the yielded item is closed, not
    /// returned to the queue and not started again.
    #[test]
    fn test_global_cancel_wins_over_deferral() {
        let queue = Queue::new();
        queue.enqueue(vec![task("a"), task("b")]);
        queue.take_next_inner();
        queue.resume_task_inner("b");
        assert_eq!(queue.inner.lock().unwrap().deferred.as_deref(), Some("a"));

        queue.cancel_inner();

        assert!(!queue.requeue_deferred_inner("a"), "отменённый пункт вернулся в очередь");
        assert_eq!(queue.outcome("b"), Some(Err(CANCELLED.into())));
    }

    /// A deferred item that managed to finish on its own clears the mark: otherwise its
    /// next cancellation would turn into a re-enqueue.
    #[test]
    fn test_deferral_is_dropped_when_the_item_finishes_on_its_own() {
        let queue = Queue::new();
        queue.enqueue(vec![task("a"), task("b")]);
        queue.take_next_inner();
        queue.resume_task_inner("b");
        queue
            .inner
            .lock()
            .unwrap()
            .set_status("a", Status::Done, None);
        assert!(queue.inner.lock().unwrap().deferred.is_none());

        assert!(queue.take_next_inner().is_some());
        queue.enqueue(vec![task("a")]);
        assert!(!queue.requeue_deferred_inner("a"));
    }

    /// Cancelling a row that has already finished does not rewrite the outcome.
    #[test]
    fn test_cancel_of_finished_row_keeps_its_outcome() {
        let queue = Queue::new();
        queue.enqueue(vec![task("a"), task("b")]);
        queue.take_next_inner();
        queue
            .inner
            .lock()
            .unwrap()
            .set_status("a", Status::Done, None);
        queue.take_next_inner();

        queue.cancel_task_inner("a");

        assert_eq!(queue.outcome("a"), Some(Ok(())));
        assert!(!queue.flags.cancelled(), "отмена готовой строки оборвала соседнюю");
    }

    /// The outcome is searched in any tag form and also after a new queue run
    /// has overwritten the list: `download_model("bge-m3")` waited for the row `bge-m3:latest`.
    #[test]
    fn test_outcome_survives_next_run_under_other_tag_form() {
        let queue = Queue::new();
        queue.enqueue(vec![task("bge-m3:latest")]);
        queue
            .inner
            .lock()
            .unwrap()
            .set_status("bge-m3:latest", Status::Done, None);
        assert!(queue.finish_inner());
        assert!(queue.enqueue(vec![task("other")]));

        assert_eq!(queue.outcome("bge-m3"), Some(Ok(())));

        // A new run under a different form forgets the past outcome.
        assert!(!queue.enqueue(vec![task("bge-m3")]));
        assert_eq!(queue.outcome("bge-m3:latest"), None);
    }

    /// The command waits for its items, not the whole queue: someone else's item behind it doesn't hold it.
    #[tokio::test]
    async fn test_wait_for_returns_when_own_items_finish() {
        let queue = Queue::new();
        queue.enqueue(vec![task("mine"), task("foreign")]);
        let waiter = {
            let queue = queue.clone();
            tokio::spawn(async move { queue.wait_for(&["mine".to_string()]).await })
        };
        // The command has time to start waiting before the item finishes.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "команда вернулась раньше своего пункта");
        queue.take_next_inner();
        queue
            .inner
            .lock()
            .unwrap()
            .set_status("mine", Status::Done, None);
        queue.take_next_inner();

        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("команда ждала чужой пункт")
            .unwrap();
        assert!(queue.snapshot().running, "очередь ещё идёт — чужой пункт качается");
    }

    #[test]
    fn test_progress_throttle_passes_changes_and_the_end() {
        let mut throttle = ProgressThrottle::new();
        assert!(throttle.due(0, 100, "качаю"), "первый тик проходит");
        assert!(!throttle.due(10, 100, "качаю"), "частый тик отсекается");
        assert!(throttle.due(20, 100, "распаковка"), "смена состояния проходит");
        assert!(throttle.due(100, 100, "распаковка"), "конец загрузки проходит");
        std::thread::sleep(EMIT_INTERVAL + Duration::from_millis(50));
        assert!(throttle.due(100, 0, "распаковка"));
    }
}
