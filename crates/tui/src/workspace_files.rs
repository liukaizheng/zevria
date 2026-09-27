//! UI-local workspace path search. No file contents or engine messages cross this boundary.
//!
//! One lazy OS thread owns traversal, the cached index, and matching. Both the
//! request slot and result channel retain only the latest value. Teardown is
//! cooperative: an OS filesystem call may block, so Drop never joins on the UI
//! thread; the retired worker drops its resources when that call returns.

use std::{
    borrow::Cow,
    collections::BinaryHeap,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread::JoinHandle,
};

use tokio::sync::watch;
use zevria_tui_input::completion::{FileCompletionRequest, FileSearchStatus};

use crate::input::PaneId;

static NEXT_SERVICE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ServiceId(u64);

impl Default for ServiceId {
    fn default() -> Self {
        Self(NEXT_SERVICE.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SearchRequest {
    pub service: ServiceId,
    pub pane: PaneId,
    pub completion: FileCompletionRequest,
}

impl SearchRequest {
    fn opening(&self) -> (PaneId, u64) {
        (self.pane, self.completion.activation)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SearchResult {
    pub request: SearchRequest,
    pub paths: Vec<String>,
    pub status: FileSearchStatus,
}

#[derive(Clone, Copy, Debug)]
struct Limits {
    files: usize,
    path_bytes: usize,
    entries: usize,
    depth: usize,
    matches: usize,
}

const LIMITS: Limits = Limits {
    files: 100_000,
    path_bytes: 32 * 1024 * 1024,
    entries: 200_000,
    depth: 64,
    matches: 50,
};

#[derive(Default)]
struct Index {
    paths: Vec<String>,
    status: FileSearchStatus,
}

#[derive(Clone, Copy)]
struct ScanOptions {
    limits: Limits,
    parents: bool,
    global: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            limits: LIMITS,
            parents: true,
            global: true,
        }
    }
}

fn selectable_path(path: &Path) -> Option<Cow<'_, str>> {
    let path = path
        .to_str()
        .filter(|path| !path.chars().any(char::is_control))?;
    // Generated references always use '/', but Unix backslashes are literal
    // filename characters. Filesystem traversal keeps using native paths.
    Some(if cfg!(windows) && path.contains('\\') {
        Cow::Owned(path.replace('\\', "/"))
    } else {
        Cow::Borrowed(path)
    })
}

fn scan(root: &Path, options: ScanOptions, cancelled: &dyn Fn() -> bool) -> Option<Index> {
    if cancelled() {
        return None;
    }
    let mut index = Index::default();
    // SessionViews captures relative constructor inputs at startup. Never
    // resolve one against a process cwd that may have changed since then.
    if !root.is_absolute() {
        index.status.unavailable = true;
        return Some(index);
    }
    let root = match root.canonicalize() {
        Ok(root) if root.is_dir() => root,
        _ => {
            index.status.unavailable = true;
            return Some(index);
        }
    };
    let limits = options.limits;
    let mut builder = ignore::WalkBuilder::new(&root);
    builder
        .current_dir(&root)
        .hidden(false)
        .parents(options.parents)
        .git_global(options.global)
        .require_git(false)
        .follow_links(false)
        .max_depth(Some(limits.depth))
        .filter_entry(|entry| entry.depth() == 0 || entry.file_name() != ".git");
    let mut bytes = 0usize;
    let mut yielded = 0usize;
    for result in builder.build().take(limits.entries) {
        if cancelled() {
            return None;
        }
        yielded += 1;
        let entry = match result {
            Ok(entry) => entry,
            Err(_) => {
                index.status.unavailable |= yielded == 1;
                index.status.errors = index.status.errors.saturating_add(1);
                continue;
            }
        };
        if entry.error().is_some() {
            index.status.errors = index.status.errors.saturating_add(1);
        }
        let Some(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            // Conservatively disclose the depth boundary, even if the directory
            // happens to be empty. We never descend to find out.
            index.status.incomplete |= entry.depth() >= limits.depth;
            continue;
        }
        if kind.is_symlink() {
            match entry.path().canonicalize() {
                Ok(target) if target.starts_with(&root) && target.is_file() => {}
                _ => {
                    index.status.omitted = index.status.omitted.saturating_add(1);
                    continue;
                }
            }
        } else if !kind.is_file() {
            continue;
        }
        let Some(path) = entry
            .path()
            .strip_prefix(&root)
            .ok()
            .and_then(selectable_path)
        else {
            index.status.omitted = index.status.omitted.saturating_add(1);
            continue;
        };
        if index.paths.len() >= limits.files || path.len() > limits.path_bytes.saturating_sub(bytes)
        {
            index.status.incomplete = true;
            break;
        }
        bytes += path.len();
        index.paths.push(path.into_owned());
    }
    index.status.incomplete |= yielded >= limits.entries;
    if cancelled() {
        return None;
    }
    index.paths.sort_unstable();
    Some(index)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Rank {
    kind: u8,
    looseness: usize,
    offset: usize,
    length: usize,
}

fn rank(
    path: &str,
    query: &str,
    query_chars: &[char],
    cancelled: &dyn Fn() -> bool,
) -> Option<Rank> {
    let folded = path.to_lowercase();
    let basename = folded.rsplit('/').next().unwrap_or(&folded);
    let length = folded.len();
    if basename == query {
        return Some(Rank {
            kind: 0,
            looseness: 0,
            offset: 0,
            length,
        });
    }
    if basename.starts_with(query) {
        return Some(Rank {
            kind: 1,
            looseness: basename.len() - query.len(),
            offset: 0,
            length,
        });
    }
    if let Some(offset) = folded.find(query) {
        return Some(Rank {
            kind: 2,
            looseness: length - query.len(),
            offset,
            length,
        });
    }
    // Greedy subsequence windows with a backward tightening pass. Continue
    // after each window's END, so windows do not overlap and total work stays
    // linear even for long, repetitive fuzzy queries. Rank the tightest of
    // these windows rather than exhaustively searching overlapping windows.
    let path_chars: Vec<char> = folded.chars().collect();
    if query_chars.len() > path_chars.len() {
        return None;
    }
    let mut best = None;
    let (mut offset, mut matched) = (0usize, 0usize);
    while offset < path_chars.len() {
        if offset % 128 == 0 && cancelled() {
            return None;
        }
        if path_chars[offset] == query_chars[matched] {
            matched += 1;
            if matched == query_chars.len() {
                let end = offset;
                matched -= 1;
                while matched > 0 {
                    if offset % 128 == 0 && cancelled() {
                        return None;
                    }
                    offset -= 1;
                    if path_chars[offset] == query_chars[matched - 1] {
                        matched -= 1;
                    }
                }
                let candidate = Rank {
                    kind: 3,
                    looseness: end - offset + 1 - query_chars.len(),
                    offset,
                    length,
                };
                best = Some(best.map_or(candidate, |previous: Rank| previous.min(candidate)));
                offset = end;
            }
        }
        offset += 1;
    }
    best
}

fn matches(
    index: &Index,
    query: &str,
    limit: usize,
    cancelled: &dyn Fn() -> bool,
) -> Option<Vec<String>> {
    if query.is_empty() {
        return (!cancelled()).then(|| index.paths.iter().take(limit).cloned().collect());
    }
    let query = query.to_lowercase();
    let query_chars: Vec<char> = query.chars().collect();
    let mut best = BinaryHeap::with_capacity(limit.saturating_add(1));
    for path in &index.paths {
        if cancelled() {
            return None;
        }
        if let Some(rank) = rank(path, &query, &query_chars, cancelled) {
            best.push((rank, path.as_str()));
            if best.len() > limit {
                best.pop();
            }
        }
    }
    (!cancelled()).then(|| {
        best.into_sorted_vec()
            .into_iter()
            .map(|(_, path)| path.to_owned())
            .collect()
    })
}

#[derive(Default)]
struct Pending {
    request: Option<SearchRequest>,
    revision: u64,
    stopped: bool,
}

#[derive(Default)]
struct Shared {
    pending: Mutex<Pending>,
    wake: Condvar,
}

impl Shared {
    fn current(&self) -> Option<SearchRequest> {
        let state = self.pending.lock().unwrap();
        (!state.stopped).then(|| state.request.clone()).flatten()
    }

    fn current_opening(&self, request: &SearchRequest) -> bool {
        let state = self.pending.lock().unwrap();
        !state.stopped
            && state
                .request
                .as_ref()
                .is_some_and(|current| current.opening() == request.opening())
    }

    fn is_current(&self, request: &SearchRequest) -> bool {
        let state = self.pending.lock().unwrap();
        !state.stopped && state.request.as_ref() == Some(request)
    }
}

type Scanner = Box<dyn FnMut(&Path, &dyn Fn() -> bool) -> Option<Index> + Send>;

pub(crate) struct FileSearchService {
    root: PathBuf,
    id: ServiceId,
    shared: Arc<Shared>,
    pub receiver: watch::Receiver<Option<SearchResult>>,
    sender: watch::Sender<Option<SearchResult>>,
    worker: Option<JoinHandle<()>>,
    scanner: Option<Scanner>,
}

impl FileSearchService {
    pub fn new(root: PathBuf, id: ServiceId) -> Self {
        let (sender, receiver) = watch::channel(None);
        Self {
            root,
            id,
            shared: Arc::new(Shared::default()),
            receiver,
            sender,
            worker: None,
            scanner: Some(Box::new(|root, cancelled| {
                scan(root, ScanOptions::default(), cancelled)
            })),
        }
    }

    pub fn set_request(&mut self, request: Option<SearchRequest>) {
        let request = request.filter(|request| request.service == self.id);
        {
            let mut pending = self.shared.pending.lock().unwrap();
            if pending.request == request {
                return;
            }
            pending.request = request.clone();
            pending.revision = pending.revision.wrapping_add(1);
        }
        if self.worker.is_none() && request.is_some() {
            let shared = Arc::clone(&self.shared);
            let root = self.root.clone();
            let sender = self.sender.clone();
            let scanner = self.scanner.take().expect("one file worker per service");
            self.worker = Some(std::thread::spawn(move || {
                worker(root, shared, sender, scanner)
            }));
        }
        self.shared.wake.notify_one();
    }
}

impl Drop for FileSearchService {
    fn drop(&mut self) {
        let mut pending = self.shared.pending.lock().unwrap();
        pending.stopped = true;
        pending.request = None;
        self.shared.wake.notify_one();
        // Dropping JoinHandle detaches; no filesystem wait on the event loop.
    }
}

fn worker(
    root: PathBuf,
    shared: Arc<Shared>,
    sender: watch::Sender<Option<SearchResult>>,
    mut scanner: Scanner,
) {
    let mut cache: Option<Index> = None;
    let mut refreshed = None;
    let mut processed = 0;
    loop {
        let (request, revision) = {
            let mut pending = shared.pending.lock().unwrap();
            while !pending.stopped && pending.revision == processed {
                pending = shared.wake.wait(pending).unwrap();
            }
            if pending.stopped {
                return;
            }
            (pending.request.clone(), pending.revision)
        };
        processed = revision;
        let Some(request) = request else { continue };
        let refresh = cache.is_none() || refreshed != Some(request.opening());
        if let Some(index) = &cache {
            publish(index, &request, refresh, &shared, &sender);
        }
        if refresh {
            // The UI retains the bounded cached suggestions while refreshing;
            // release the old full index before building another 32 MiB one.
            cache = None;
            let Some(index) = scanner(&root, &|| !shared.current_opening(&request)) else {
                continue;
            };
            if !shared.current_opening(&request) {
                continue;
            }
            cache = Some(index);
            refreshed = Some(request.opening());
            // Keystrokes during traversal coalesce; match only the latest query.
            let Some(latest) = shared.current() else {
                continue;
            };
            if latest.opening() == request.opening() {
                publish(cache.as_ref().unwrap(), &latest, false, &shared, &sender);
            }
        }
    }
}

fn publish(
    index: &Index,
    request: &SearchRequest,
    refreshing: bool,
    shared: &Shared,
    sender: &watch::Sender<Option<SearchResult>>,
) {
    let Some(paths) = matches(
        index,
        &request.completion.identity.query.prefix,
        LIMITS.matches,
        &|| !shared.is_current(request),
    ) else {
        return;
    };
    if !shared.is_current(request) {
        return;
    }
    let mut status = index.status.clone();
    status.loading = refreshing;
    sender.send_replace(Some(SearchResult {
        request: request.clone(),
        paths,
        status,
    }));
}

#[cfg(test)]
mod tests;
