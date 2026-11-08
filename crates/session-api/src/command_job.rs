//! Session-owned bounded shell jobs. No live state here enters model instructions.
use crate::{
    CommandControl, CommandControlAction, CommandControlResult, SessionEvent, SessionEventSender,
    TurnId,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
    sync::{Arc, Mutex, Weak},
    time::{Duration, SystemTime},
};
use tokio::{sync::watch, task::AbortHandle, time::Instant};
use tokio_util::sync::CancellationToken;
use zevria_foundation::{
    CommandJobId, CommandJobMode, CommandJobSnapshot, CommandJobStatus, CommandJobView,
    TerminationOrigin,
};

#[derive(Debug, Clone, Copy)]
pub struct CommandJobLimits {
    pub max_running_background: usize,
    pub retained_finished: usize,
    pub capture_bytes: usize,
    pub foreground_timeout: Duration,
    pub background_timeout: Option<Duration>,
}

/// Lossy UTF-8 segments with the raw byte count discarded between them.
/// Keeping capture gaps structured lets the model renderer account for all elision.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamText {
    pub head: String,
    pub discarded: usize,
    pub tail: String,
}
impl StreamText {
    pub fn is_empty(&self) -> bool {
        self.head.is_empty() && self.discarded == 0 && self.tail.is_empty()
    }
}

/// Head/tail retention with absolute offsets, independent frontend/model readers.
#[derive(Debug, Clone)]
pub struct StreamBuffer {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    limit: usize,
    total: usize,
    cursor: usize,
}
impl StreamBuffer {
    pub fn new(limit: usize) -> Self {
        Self {
            head: Vec::new(),
            tail: VecDeque::new(),
            limit,
            total: 0,
            cursor: 0,
        }
    }
    pub fn append(&mut self, mut bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len());
        let take = (self.limit / 2 - self.head.len()).min(bytes.len());
        self.head.extend_from_slice(&bytes[..take]);
        bytes = &bytes[take..];
        let tail_limit = self.limit - self.limit / 2;
        for &byte in bytes {
            if self.tail.len() == tail_limit && tail_limit > 0 {
                self.tail.pop_front();
            }
            if tail_limit > 0 {
                self.tail.push_back(byte);
            }
        }
    }
    pub fn total_bytes(&self) -> usize {
        self.total
    }
    pub fn unread_len(&self) -> usize {
        self.total.saturating_sub(self.cursor)
    }
    pub fn retained(&self) -> StreamText {
        let retained = self.head.len() + self.tail.len();
        if self.total > retained {
            return StreamText {
                head: String::from_utf8_lossy(&self.head).into_owned(),
                discarded: self.total - retained,
                tail: String::from_utf8_lossy(&self.tail.iter().copied().collect::<Vec<_>>())
                    .into_owned(),
            };
        }
        // Without a gap, decode together so a retention boundary cannot split UTF-8.
        let bytes: Vec<_> = self.head.iter().chain(self.tail.iter()).copied().collect();
        StreamText {
            head: String::from_utf8_lossy(&bytes).into_owned(),
            ..StreamText::default()
        }
    }
    pub fn peek_unread(&self, running: bool) -> StreamText {
        self.clone().take_unread(running)
    }
    pub fn consume_all(&mut self) {
        self.cursor = self.total;
    }
    pub fn take_unread(&mut self, running: bool) -> StreamText {
        let tail_start = self.total - self.tail.len();
        let mut bytes = Vec::new();
        if self.cursor < self.head.len() {
            bytes.extend_from_slice(&self.head[self.cursor..]);
        }
        let gap_start = self.cursor.max(self.head.len());
        if tail_start > gap_start {
            // Decode segments independently at a discarded gap, as in retained output.
            let head = String::from_utf8_lossy(&bytes).into_owned();
            bytes = self.tail.iter().copied().collect();
            let end = utf8_end(&bytes, running);
            self.cursor = tail_start + end;
            return StreamText {
                head,
                discarded: tail_start - gap_start,
                tail: String::from_utf8_lossy(&bytes[..end]).into_owned(),
            };
        }
        bytes.extend(
            self.tail
                .iter()
                .skip(self.cursor.saturating_sub(tail_start))
                .copied(),
        );
        let start = self.cursor;
        let end = utf8_end(&bytes, running);
        self.cursor = start + end;
        StreamText {
            head: String::from_utf8_lossy(&bytes[..end]).into_owned(),
            ..StreamText::default()
        }
    }
    pub fn tail_preview(&self, max: usize) -> String {
        let mut bytes: Vec<u8> = if self.total > self.head.len() + self.tail.len() {
            self.tail.iter().copied().collect()
        } else {
            self.head.iter().chain(self.tail.iter()).copied().collect()
        };
        if bytes.len() > max {
            bytes.drain(..bytes.len() - max);
        }
        let mut start = 0;
        while start < bytes.len() && bytes[start] & 0xc0 == 0x80 {
            start += 1;
        }
        let text = String::from_utf8_lossy(&bytes[start..]);
        let mut start = text.len().saturating_sub(max);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        text[start..].to_owned()
    }
}
fn utf8_end(bytes: &[u8], running: bool) -> usize {
    if running {
        let mut offset = 0;
        while offset < bytes.len() {
            match std::str::from_utf8(&bytes[offset..]) {
                Ok(_) => break,
                Err(e) if e.error_len().is_none() => return offset + e.valid_up_to(),
                Err(e) => offset += e.valid_up_to() + e.error_len().unwrap(),
            }
        }
    }
    bytes.len()
}

#[derive(Debug, Clone, Copy)]
pub enum CommandStream {
    Stdout,
    Stderr,
}
#[derive(Debug, Clone)]
pub struct RegisterError(pub String);
impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for RegisterError {}

#[derive(Clone)]
pub struct CommandJobs(Arc<Inner>);
impl fmt::Debug for CommandJobs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandJobs")
            .field("limits", &self.0.limits)
            .finish_non_exhaustive()
    }
}
struct Inner {
    limits: CommandJobLimits,
    events: Option<SessionEventSender>,
    state: Mutex<State>,
    completions_changed: watch::Sender<u64>,
}
#[derive(Default)]
struct State {
    closed: bool,
    jobs: HashMap<CommandJobId, Arc<Job>>,
    // Never reuse an ID in this session, even after eviction.
    used_ids: HashSet<CommandJobId>,
    finished: VecDeque<CommandJobId>,
    completions: VecDeque<Arc<Job>>,
}
struct Job {
    record: Mutex<Record>,
    changed: watch::Sender<u64>,
    termination: watch::Sender<Option<TerminationOrigin>>,
    deadline: watch::Sender<Option<Instant>>,
    publication: tokio::sync::Mutex<()>,
}
struct Record {
    snapshot: CommandJobSnapshot,
    stdout: StreamBuffer,
    stderr: StreamBuffer,
    started: Instant,
    model_owned: bool,
    origin: Option<TurnId>,
    // A status-only notice promises that command_status can still read output.
    preserve_unread: bool,
    pending_mode: Option<CommandJobSnapshot>,
    supervisor: Option<AbortHandle>,
}
impl Job {
    fn changed(&self) {
        self.changed
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }
    fn snapshot(&self) -> CommandJobSnapshot {
        self.record.lock().unwrap().snapshot.clone()
    }
    fn unread(&self) -> (StreamText, StreamText) {
        let mut r = self.record.lock().unwrap();
        let running = !r.snapshot.status.is_terminal();
        (r.stdout.take_unread(running), r.stderr.take_unread(running))
    }
    fn terminate(&self, origin: TerminationOrigin, foreground_only: bool) -> bool {
        let r = self.record.lock().unwrap();
        if !r.snapshot.status.is_terminal()
            && (!foreground_only || r.snapshot.mode == CommandJobMode::Foreground)
        {
            self.termination.send_if_modified(|requested| {
                if requested.is_some() {
                    false
                } else {
                    *requested = Some(origin);
                    true
                }
            });
            return true;
        }
        false
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        for job in self.state.get_mut().unwrap().jobs.values() {
            if let Some(supervisor) = &job.record.lock().unwrap().supervisor {
                supervisor.abort();
            }
        }
    }
}

impl CommandJobs {
    pub fn new(limits: CommandJobLimits, events: Option<SessionEventSender>) -> Self {
        Self(Arc::new(Inner {
            limits,
            events,
            state: Mutex::new(State::default()),
            completions_changed: watch::channel(0).0,
        }))
    }
    pub fn private(mut limits: CommandJobLimits) -> Self {
        limits.max_running_background = 0;
        limits.retained_finished = 0;
        Self::new(limits, None)
    }
    /// Keep display IDs in restored tool sidecars from ever aliasing new work.
    /// This reserves identities only; it does not restore runtime jobs.
    pub fn reserve_ids(&self, ids: impl IntoIterator<Item = CommandJobId>) {
        self.0.state.lock().unwrap().used_ids.extend(ids);
    }
    pub fn limits(&self) -> CommandJobLimits {
        self.0.limits
    }
    pub fn background_enabled(&self) -> bool {
        self.0.limits.max_running_background > 0
    }
    pub async fn register(
        &self,
        command: String,
        call_id: Option<String>,
        mode: CommandJobMode,
        origin: Option<TurnId>,
    ) -> Result<JobHandle, RegisterError> {
        let job = {
            let mut s = self.0.state.lock().unwrap();
            if s.closed {
                return Err(RegisterError(
                    "command job registry is closed; this session is shutting down".into(),
                ));
            }
            if mode == CommandJobMode::Background {
                self.check_capacity(&s)?;
            }
            let id = loop {
                let id = CommandJobId::generate();
                if s.used_ids.insert(id.clone()) {
                    break id;
                }
            };
            let timeout = if mode == CommandJobMode::Foreground {
                Some(self.0.limits.foreground_timeout)
            } else {
                self.0.limits.background_timeout
            };
            let job = Arc::new(Job {
                record: Mutex::new(Record {
                    snapshot: CommandJobSnapshot {
                        id: id.clone(),
                        call_id,
                        command: command.chars().take(512).collect(),
                        mode,
                        status: CommandJobStatus::Running,
                        started_at: SystemTime::now(),
                        finished_at: None,
                        stdout_bytes: 0,
                        stderr_bytes: 0,
                    },
                    stdout: StreamBuffer::new(self.0.limits.capture_bytes / 2),
                    stderr: StreamBuffer::new(self.0.limits.capture_bytes / 2),
                    started: Instant::now(),
                    model_owned: mode == CommandJobMode::Background,
                    origin,
                    preserve_unread: false,
                    pending_mode: None,
                    supervisor: None,
                }),
                changed: watch::channel(0).0,
                termination: watch::channel(None).0,
                deadline: watch::channel(timeout.map(|d| Instant::now() + d)).0,
                publication: tokio::sync::Mutex::new(()),
            });
            s.jobs.insert(id, job.clone());
            job
        };
        let handle = JobHandle {
            registry: Arc::downgrade(&self.0),
            job,
        };
        let mut registration = RegistrationGuard {
            registry: Arc::downgrade(&self.0),
            id: handle.snapshot().id,
            committed: false,
        };
        handle.publish(handle.snapshot()).await;
        registration.committed = true;
        Ok(handle)
    }
    fn check_capacity(&self, s: &State) -> Result<(), RegisterError> {
        if !self.background_enabled() {
            return Err(RegisterError("background commands are disabled".into()));
        }
        let count = s
            .jobs
            .values()
            .filter(|job| {
                let r = job.record.lock().unwrap();
                r.snapshot.mode == CommandJobMode::Background && !r.snapshot.status.is_terminal()
            })
            .count();
        if count >= self.0.limits.max_running_background {
            return Err(RegisterError(format!(
                "running background job cap ({}) reached; use command_status and command_terminate before starting another job",
                self.0.limits.max_running_background
            )));
        }
        Ok(())
    }
    fn get(&self, id: &CommandJobId) -> Result<Arc<Job>, RegisterError> {
        self.0.state.lock().unwrap().jobs.get(id).cloned().ok_or_else(|| RegisterError(format!("unknown command job {id}; use command_status without an id to list jobs. Jobs do not survive restarts or session replacement.")))
    }
    pub fn list_model_jobs(&self) -> Vec<CommandJobSnapshot> {
        let s = self.0.state.lock().unwrap();
        let mut jobs: Vec<_> = s
            .jobs
            .values()
            .filter_map(|j| {
                let r = j.record.lock().unwrap();
                r.model_owned.then(|| r.snapshot.clone())
            })
            .collect();
        jobs.sort_by_key(|j| j.started_at);
        jobs
    }
    /// Pin one job for an in-flight read/termination even if retention evicts it.
    pub fn job(&self, id: &CommandJobId) -> Result<JobHandle, RegisterError> {
        Ok(JobHandle {
            registry: Arc::downgrade(&self.0),
            job: self.get(id)?,
        })
    }
    pub fn snapshot(&self, id: &CommandJobId) -> Result<CommandJobSnapshot, RegisterError> {
        Ok(self.get(id)?.snapshot())
    }
    pub fn take_unread(
        &self,
        id: &CommandJobId,
    ) -> Result<(StreamText, StreamText), RegisterError> {
        Ok(self.get(id)?.unread())
    }
    pub fn terminate(
        &self,
        id: &CommandJobId,
        origin: TerminationOrigin,
    ) -> Result<CommandJobSnapshot, RegisterError> {
        let job = self.get(id)?;
        job.terminate(origin, false);
        Ok(job.snapshot())
    }
    pub fn move_to_background(
        &self,
        id: &CommandJobId,
    ) -> Result<CommandJobSnapshot, RegisterError> {
        let s = self.0.state.lock().unwrap();
        let job = s
            .jobs
            .get(id)
            .ok_or_else(|| RegisterError(format!("unknown command job {id}")))?;
        self.check_capacity(&s)?;
        let mut r = job.record.lock().unwrap();
        if r.snapshot.status.is_terminal()
            || r.snapshot.mode != CommandJobMode::Foreground
            || job.termination.borrow().is_some()
        {
            return Err(RegisterError(
                "only a running foreground command can be moved to the background".into(),
            ));
        }
        r.snapshot.mode = CommandJobMode::Background;
        r.model_owned = true;
        r.pending_mode = Some(r.snapshot.clone());
        job.deadline
            .send_replace(self.0.limits.background_timeout.map(|d| r.started + d));
        let snapshot = r.snapshot.clone();
        drop(r);
        job.changed();
        Ok(snapshot)
    }
    pub fn handle_control(&self, control: CommandControl) -> CommandControlResult {
        let result = match control.action {
            CommandControlAction::Inspect { id } => {
                let jobs: Result<Vec<_>, _> = if let Some(id) = &id {
                    self.get(id).map(|j| vec![j])
                } else {
                    Ok(self
                        .0
                        .state
                        .lock()
                        .unwrap()
                        .jobs
                        .values()
                        .cloned()
                        .collect())
                };
                jobs.map(|jobs| {
                    let mut views: Vec<_> = jobs
                        .into_iter()
                        .map(|j| {
                            let r = j.record.lock().unwrap();
                            CommandJobView {
                                snapshot: r.snapshot.clone(),
                                stdout_tail: if id.is_some() {
                                    r.stdout.tail_preview(8192)
                                } else {
                                    String::new()
                                },
                                stderr_tail: if id.is_some() {
                                    r.stderr.tail_preview(8192)
                                } else {
                                    String::new()
                                },
                            }
                        })
                        .collect();
                    views.sort_by_key(|v| v.snapshot.started_at);
                    CommandControlResult::Inspected(views)
                })
            }
            CommandControlAction::Terminate { id } => self
                .terminate(&id, TerminationOrigin::User)
                .map(CommandControlResult::Accepted),
            CommandControlAction::MoveToBackground { id } => self
                .move_to_background(&id)
                .map(CommandControlResult::Accepted),
        };
        result.unwrap_or_else(|e| CommandControlResult::Rejected {
            message: e.to_string(),
        })
    }
    pub async fn wait_terminal(
        &self,
        id: &CommandJobId,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<bool, RegisterError> {
        Ok(self.job(id)?.wait_terminal(timeout, cancellation).await)
    }
    /// Finish-order snapshots pin unread streams until a delivery is committed.
    pub fn pending_completions(&self) -> Vec<PendingCompletion> {
        self.0
            .state
            .lock()
            .unwrap()
            .completions
            .iter()
            .map(|job| {
                let r = job.record.lock().unwrap();
                PendingCompletion {
                    snapshot: r.snapshot.clone(),
                    origin: r.origin,
                    job: job.clone(),
                }
            })
            .collect()
    }
    pub fn acknowledge_completions(&self, acknowledgements: &[(CommandJobId, bool)]) {
        let mut state = self.0.state.lock().unwrap();
        for (id, consumed) in acknowledgements {
            if let Some(job) = state
                .completions
                .iter()
                .find(|job| job.snapshot().id == *id)
            {
                let mut r = job.record.lock().unwrap();
                if *consumed {
                    r.stdout.consume_all();
                    r.stderr.consume_all();
                }
                r.preserve_unread =
                    !consumed && (r.stdout.unread_len() > 0 || r.stderr.unread_len() > 0);
            }
            state.completions.retain(|job| job.snapshot().id != *id);
        }
        evict_finished(&mut state, self.0.limits.retained_finished);
    }
    pub fn completions_changed(&self) -> watch::Receiver<u64> {
        self.0.completions_changed.subscribe()
    }
    pub fn running_model_jobs(&self) -> Vec<CommandJobSnapshot> {
        self.list_model_jobs()
            .into_iter()
            .filter(|job| !job.status.is_terminal())
            .collect()
    }
    pub fn mark_reported(&self, id: &CommandJobId) {
        let mut state = self.0.state.lock().unwrap();
        state.completions.retain(|job| &job.snapshot().id != id);
        if let Some(job) = state.jobs.get(id) {
            job.record.lock().unwrap().preserve_unread = false;
        }
        evict_finished(&mut state, self.0.limits.retained_finished);
    }
    pub fn is_closed(&self) -> bool {
        self.0.state.lock().unwrap().closed
    }
    pub fn close(&self) {
        let mut state = self.0.state.lock().unwrap();
        if !state.closed {
            state.closed = true;
            // Wake an idle listener so it can retire the closed registry.
            self.0
                .completions_changed
                .send_modify(|revision| *revision = revision.wrapping_add(1));
        }
    }
    pub async fn shutdown(&self, grace: Duration) {
        self.close();
        let jobs: Vec<_> = self
            .0
            .state
            .lock()
            .unwrap()
            .jobs
            .values()
            .cloned()
            .collect();
        for job in &jobs {
            job.terminate(TerminationOrigin::Shutdown, false);
        }
        let tasks: Vec<_> = jobs
            .iter()
            .filter_map(|job| job.record.lock().unwrap().supervisor.clone())
            .collect();
        let wait = async {
            while jobs.iter().any(|job| !job.snapshot().status.is_terminal())
                || tasks.iter().any(|task| !task.is_finished())
            {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        };
        let _ = tokio::time::timeout(grace, wait).await;
        for task in &tasks {
            if !task.is_finished() {
                task.abort();
            }
        }
        // Abort completion, not just its request, guarantees drop guards ran.
        while tasks.iter().any(|task| !task.is_finished()) {
            tokio::task::yield_now().await;
        }
        for job in jobs {
            let snapshot = {
                let mut r = job.record.lock().unwrap();
                if r.snapshot.status.is_terminal() {
                    continue;
                }
                r.snapshot.status = CommandJobStatus::Terminated {
                    origin: TerminationOrigin::Shutdown,
                };
                r.snapshot.finished_at = Some(SystemTime::now());
                r.snapshot.clone()
            };
            job.changed();
            if let Some(events) = &self.0.events {
                let _ = events.try_send(SessionEvent::CommandJobUpdated { job: snapshot });
            }
        }
    }
}

/// A non-consuming delivery candidate. Its pinned job survives retention.
#[derive(Clone)]
pub struct PendingCompletion {
    pub snapshot: CommandJobSnapshot,
    pub origin: Option<TurnId>,
    job: Arc<Job>,
}
impl PendingCompletion {
    pub fn peek_unread(&self) -> (StreamText, StreamText) {
        let r = self.job.record.lock().unwrap();
        (r.stdout.peek_unread(false), r.stderr.peek_unread(false))
    }
    pub fn unread_bytes(&self) -> (usize, usize) {
        let r = self.job.record.lock().unwrap();
        (r.stdout.unread_len(), r.stderr.unread_len())
    }
}

fn evict_finished(state: &mut State, limit: usize) {
    while state.finished.len() > limit {
        let Some(index) = state.finished.iter().position(|id| {
            !state.completions.iter().any(|job| job.snapshot().id == *id)
                && state
                    .jobs
                    .get(id)
                    .is_none_or(|job| !job.record.lock().unwrap().preserve_unread)
        }) else {
            break;
        };
        let id = state.finished.remove(index).expect("existing finished job");
        state.jobs.remove(&id);
    }
}

struct RegistrationGuard {
    registry: Weak<Inner>,
    id: CommandJobId,
    committed: bool,
}
impl Drop for RegistrationGuard {
    fn drop(&mut self) {
        if !self.committed
            && let Some(registry) = self.registry.upgrade()
        {
            registry.state.lock().unwrap().jobs.remove(&self.id);
        }
    }
}

/// Supervisor/foreground-waiter capability; only a weak reference to the registry.
#[derive(Clone)]
pub struct JobHandle {
    registry: Weak<Inner>,
    job: Arc<Job>,
}
impl JobHandle {
    pub async fn wait_terminal(&self, timeout: Duration, cancellation: &CancellationToken) -> bool {
        let mut changed = self.changes();
        let wait = async {
            loop {
                if self.snapshot().status.is_terminal() {
                    return;
                }
                if changed.changed().await.is_err() {
                    return;
                }
            }
        };
        tokio::select! { biased; () = cancellation.cancelled() => false, () = wait => true, () = tokio::time::sleep(timeout) => false }
    }
    pub fn snapshot(&self) -> CommandJobSnapshot {
        self.job.snapshot()
    }
    pub fn take_unread(&self) -> (StreamText, StreamText) {
        self.job.unread()
    }
    pub fn retained(&self) -> (StreamText, StreamText) {
        let r = self.job.record.lock().unwrap();
        (r.stdout.retained(), r.stderr.retained())
    }
    pub fn append(&self, stream: CommandStream, bytes: &[u8]) {
        let mut r = self.job.record.lock().unwrap();
        match stream {
            CommandStream::Stdout => {
                r.stdout.append(bytes);
                r.snapshot.stdout_bytes = r.stdout.total_bytes();
            }
            CommandStream::Stderr => {
                r.stderr.append(bytes);
                r.snapshot.stderr_bytes = r.stderr.total_bytes();
            }
        }
    }
    pub fn set_supervisor(&self, supervisor: AbortHandle) {
        self.job.record.lock().unwrap().supervisor = Some(supervisor);
    }
    pub fn termination_requested(&self) -> watch::Receiver<Option<TerminationOrigin>> {
        self.job.termination.subscribe()
    }
    pub fn deadline(&self) -> watch::Receiver<Option<Instant>> {
        self.job.deadline.subscribe()
    }
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.job.changed.subscribe()
    }
    pub fn deadline_seconds(&self, deadline: Instant) -> u64 {
        deadline
            .saturating_duration_since(self.job.record.lock().unwrap().started)
            .as_secs()
    }
    pub fn terminate(&self, origin: TerminationOrigin) {
        self.job.terminate(origin, false);
    }
    /// Serialize turn cancellation/abandonment with user backgrounding.
    pub fn terminate_foreground(&self, origin: TerminationOrigin) -> bool {
        self.job.terminate(origin, true)
    }
    pub async fn publish_mode_change(&self) {
        let _publication = self.job.publication.lock().await;
        let snapshot = self.job.record.lock().unwrap().pending_mode.take();
        if let Some(snapshot) = snapshot {
            self.publish(snapshot).await;
        }
    }
    async fn publish(&self, job: CommandJobSnapshot) {
        let events = self.registry.upgrade().and_then(|r| r.events.clone());
        if let Some(events) = events {
            let _ = events.send(SessionEvent::CommandJobUpdated { job }).await;
        }
    }
    /// Launch failure rollback. The terminal event lets a frontend remove its running row.
    pub async fn launch_failed(&self, error: String) {
        self.job.record.lock().unwrap().model_owned = false;
        self.finish(CommandJobStatus::Failed { error }).await;
        if let Some(registry) = self.registry.upgrade() {
            registry
                .state
                .lock()
                .unwrap()
                .jobs
                .remove(&self.snapshot().id);
        }
    }
    pub async fn finish(&self, status: CommandJobStatus) {
        let snapshot = {
            let registry = self.registry.upgrade();
            // Registry-before-record is the global lock order. Publish terminal
            // state and its notice atomically with respect to model reads, so a
            // status result cannot mark a not-yet-inserted notice as reported.
            let mut state = registry.as_ref().map(|r| r.state.lock().unwrap());
            let mut r = self.job.record.lock().unwrap();
            if r.snapshot.status.is_terminal() {
                return;
            }
            r.snapshot.status = status;
            r.snapshot.finished_at = Some(SystemTime::now());
            let snapshot = r.snapshot.clone();
            let model_owned = r.model_owned;
            drop(r);
            if let Some(s) = state.as_mut() {
                if model_owned {
                    if !s.closed {
                        s.completions.push_back(self.job.clone());
                        registry
                            .as_ref()
                            .unwrap()
                            .completions_changed
                            .send_modify(|revision| *revision = revision.wrapping_add(1));
                    }
                    s.finished.push_back(snapshot.id.clone());
                    evict_finished(s, registry.as_ref().unwrap().limits.retained_finished);
                } else {
                    s.jobs.remove(&snapshot.id);
                }
            }
            snapshot
        };
        self.job.changed();
        let _publication = self.job.publication.lock().await;
        let mode = self.job.record.lock().unwrap().pending_mode.take();
        if let Some(mode) = mode {
            self.publish(mode).await;
        }
        self.publish(snapshot).await;
    }
}

#[cfg(test)]
#[path = "command_completion_tests.rs"]
mod completion_tests;
#[cfg(test)]
#[path = "command_job_tests.rs"]
mod edge_tests;

#[cfg(test)]
mod tests {
    use super::*;
    fn limits() -> CommandJobLimits {
        CommandJobLimits {
            max_running_background: 1,
            retained_finished: 1,
            capture_bytes: 1024,
            foreground_timeout: Duration::from_secs(30),
            background_timeout: None,
        }
    }
    #[test]
    fn bounded_buffer_parity_cursor_gaps_utf8_and_frontend_independence() {
        let mut b = StreamBuffer::new(8);
        b.append(b"HEAD123456TAIL");
        let expected = StreamText {
            head: "HEAD".into(),
            discarded: 6,
            tail: "TAIL".into(),
        };
        assert_eq!(b.retained(), expected);
        assert_eq!(b.take_unread(false), expected);
        assert!(b.take_unread(false).is_empty());
        b.append(b"END");
        assert_eq!(b.tail_preview(3), "END");
        assert_eq!(
            b.take_unread(true),
            StreamText {
                head: "END".into(),
                ..StreamText::default()
            }
        );
        let mut b = StreamBuffer::new(64);
        b.append(&[0xe2, 0x82]);
        assert!(b.take_unread(true).is_empty());
        b.append(&[0xac]);
        assert_eq!(
            b.take_unread(true),
            StreamText {
                head: "€".into(),
                ..StreamText::default()
            }
        );
        assert_eq!(b.unread_len(), 0);
        b.append(&[0xe2]);
        assert_eq!(
            b.take_unread(false),
            StreamText {
                head: "�".into(),
                ..StreamText::default()
            }
        );
    }
    #[tokio::test]
    async fn controls_limits_eviction_notices_and_close() {
        let jobs = CommandJobs::new(limits(), None);
        let fg = jobs
            .register("fg".into(), None, CommandJobMode::Foreground, None)
            .await
            .unwrap();
        let id = fg.snapshot().id;
        assert!(
            matches!(jobs.handle_control(CommandControl { request_id: "i".into(), action: CommandControlAction::Inspect { id: None } }), CommandControlResult::Inspected(v) if v.len() == 1 && v[0].stdout_tail.is_empty())
        );
        jobs.move_to_background(&id).unwrap();
        assert!(
            jobs.register("over cap".into(), None, CommandJobMode::Background, None)
                .await
                .is_err()
        );
        assert!(jobs.move_to_background(&id).is_err());
        jobs.terminate(&id, TerminationOrigin::User).unwrap();
        assert_eq!(
            *fg.termination_requested().borrow(),
            Some(TerminationOrigin::User)
        );
        fg.finish(CommandJobStatus::Terminated {
            origin: TerminationOrigin::User,
        })
        .await;
        assert_eq!(jobs.pending_completions().len(), 1);
        jobs.acknowledge_completions(&[(id.clone(), true)]);
        assert!(jobs.pending_completions().is_empty());
        let next = jobs
            .register("next".into(), None, CommandJobMode::Background, None)
            .await
            .unwrap();
        assert_ne!(id, next.snapshot().id);
        next.finish(CommandJobStatus::Exited {
            code: Some(0),
            success: true,
            signal: None,
        })
        .await;
        assert!(jobs.snapshot(&id).is_err());
        assert_eq!(jobs.list_model_jobs().len(), 1);
        jobs.close();
        assert!(
            jobs.register("closed".into(), None, CommandJobMode::Foreground, None)
                .await
                .is_err()
        );
        assert!(matches!(
            jobs.handle_control(CommandControl {
                request_id: "x".into(),
                action: CommandControlAction::Terminate { id }
            }),
            CommandControlResult::Rejected { .. }
        ));
    }
    #[tokio::test]
    async fn event_order_and_shutdown() {
        let (events, mut receiver) = crate::session_event_channel(8);
        let jobs = CommandJobs::new(limits(), Some(events));
        let handle = jobs
            .register("test".into(), None, CommandJobMode::Foreground, None)
            .await
            .unwrap();
        jobs.move_to_background(&handle.snapshot().id).unwrap();
        handle
            .finish(CommandJobStatus::Exited {
                code: Some(0),
                success: true,
                signal: None,
            })
            .await;
        let mut states = Vec::new();
        for _ in 0..3 {
            if let Some(crate::SessionUpdate::Lifecycle(SessionEvent::CommandJobUpdated { job })) =
                receiver.recv().await
            {
                states.push((job.mode, job.status.is_terminal()));
            }
        }
        assert_eq!(
            states,
            vec![
                (CommandJobMode::Foreground, false),
                (CommandJobMode::Background, false),
                (CommandJobMode::Background, true)
            ]
        );
        let handle = jobs
            .register("shutdown".into(), None, CommandJobMode::Background, None)
            .await
            .unwrap();
        jobs.shutdown(Duration::ZERO).await;
        assert_eq!(
            *handle.termination_requested().borrow(),
            Some(TerminationOrigin::Shutdown)
        );
    }
}
