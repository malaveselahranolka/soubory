//! Multi-volume background indexing, watching, snapshot persistence, and ranked search service.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Write};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use fm_fs::attrs::DIRECTORY;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    FlushFileBuffers, GetDriveTypeW, GetLogicalDriveStringsW, GetVolumeInformationW,
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::core::PCWSTR;

use crate::arena::Index;
use crate::query::{CancelToken, SearchOptions, parse_query};
use crate::win::walker::{
    DirSource, WalkBatch, WalkFolderRef, WalkFoldersReport, WalkOptions, Win32DirSource,
    merge_index_with_report, walk_with_source_streaming,
};
use crate::win::watcher::{
    DeniedFilter, NoopWatcherFs, NotifyRecord, VolumeWatcher, WatcherConfig, Win32WatcherFs,
    batch_needs_prefetch, collect_parent_hints, prefetch_batch_outside_lock,
    renames_resolved_in_index, replay_records_filtered,
};
use crate::win::{
    is_ntfs_volume, join_child_path, lock_or_recover, normalize_display_path, open_dir_handle,
    query_dir_info_by_handle, read_or_recover, to_extended_path, wait_or_recover,
    wait_timeout_or_recover, wide_null, win32_to_io, write_or_recover,
};

const DRIVE_FIXED: u32 = 3;
/// How often a first walk moves what it has listed into the searchable index.
const PROGRESSIVE_INTERVAL: Duration = Duration::from_millis(500);
const HOURLY_SAVE_INTERVAL: Duration = Duration::from_secs(3600);
const DAILY_REWALK_INTERVAL: Duration = Duration::from_secs(86400);
const HOUSEKEEP_TICK: Duration = Duration::from_secs(60);
const MAX_DURING_WALK_BUFFER: usize = 100_000;
/// Minimum delay before retrying a failed walk, even with a zero overflow cooldown.
#[cfg(not(test))]
const FAILED_WALK_RETRY_MIN: Duration = Duration::from_secs(5);
#[cfg(test)]
const FAILED_WALK_RETRY_MIN: Duration = Duration::from_millis(100);
/// Cap of the doubling retry delay for walks that keep failing (missing root, walker panic).
const FAILED_WALK_RETRY_MAX: Duration = Duration::from_secs(3600);

static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

/// Lifecycle and live-update events emitted by [`Service`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Periodic progress update (~100 ms) during an initial walk or re-walk.
    Progress {
        /// Volume or directory root being scanned.
        root: String,
        /// Number of entries discovered so far.
        entries: usize,
    },
    /// A walk or background re-walk finished for `root`.
    Ready {
        /// Volume or directory root that finished scanning.
        root: String,
        /// Total indexed entries for `root` after merge.
        entries: usize,
        /// Wall-clock scan duration in milliseconds.
        millis: u64,
    },
    /// Live watcher changes or a background re-walk updated the index.
    Changed,
    /// Non-fatal background error (e.g. watcher start or re-arm failure); watcher retries with backoff.
    Error {
        /// Volume or directory root where the error occurred.
        root: String,
        /// Diagnostic error description.
        message: String,
    },
}

/// Ranked search result returned by [`Service::search`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Full display path of the matched item.
    pub path: String,
    /// Display filename (leaf component).
    pub name: String,
    /// `true` if the item is a directory.
    pub is_dir: bool,
    /// Logical file size in bytes (`0` for directories).
    pub size: u64,
    /// Last modification time in Windows `FILETIME` ticks.
    pub mtime: i64,
    /// Win32 `FILE_ATTRIBUTE_*` bitmask.
    pub attrs: u32,
}

struct RewalkCoord {
    running: bool,
    manual_or_timer_pending: bool,
    overflow_pending: bool,
    last_finished: Option<Instant>,
    stop: bool,
    collecting: bool,
    during_walk: Vec<NotifyRecord>,
    overflowed_during_walk: bool,
    /// Consecutive failed walks; drives the doubling retry delay.
    failed_streak: u32,
}

/// Retry delay after `streak` consecutive failed walks: doubles from
/// `max(cooldown, FAILED_WALK_RETRY_MIN)` up to [`FAILED_WALK_RETRY_MAX`].
fn failed_walk_cooldown(cooldown: Duration, streak: u32) -> Duration {
    cooldown
        .max(FAILED_WALK_RETRY_MIN)
        .saturating_mul(1u32 << streak.saturating_sub(1).min(16))
        .min(FAILED_WALK_RETRY_MAX)
}

/// Captures records the watcher just applied while a walk runs, for replay after the merge.
/// Nothing is captured after a gap (overflow or watcher restart): pairing records across a gap
/// turns a lost rename into a removal.
fn capture_during_walk(coord: &Mutex<RewalkCoord>, records: &[NotifyRecord]) {
    let mut st = lock_or_recover(coord);
    if !st.collecting || st.overflowed_during_walk {
        return;
    }
    if st.during_walk.len().saturating_add(records.len()) <= MAX_DURING_WALK_BUFFER {
        st.during_walk.extend_from_slice(records);
    } else {
        st.overflowed_during_walk = true;
        st.overflow_pending = true;
    }
}

/// The watcher lost events (buffer overflow, or it restarted after an error): schedule a
/// re-walk through the overflow cooldown and stop capturing for the running walk's replay.
fn note_watcher_gap(coord: &(Mutex<RewalkCoord>, Condvar)) {
    let (mu, cv) = coord;
    let mut st = lock_or_recover(mu);
    if !st.stop {
        st.overflow_pending = true;
        if st.collecting {
            st.overflowed_during_walk = true;
        }
        cv.notify_one();
    }
}

/// The watcher re-armed after an error: the root is reachable again, so a failed-walk backoff
/// (up to an hour) must not delay the recovery walk beyond the normal overflow cooldown.
fn note_watcher_restarted(coord: &(Mutex<RewalkCoord>, Condvar)) {
    lock_or_recover(&coord.0).failed_streak = 0;
    note_watcher_gap(coord);
}

struct VolumeState {
    volume_serial: u32,
    index: Arc<RwLock<Index>>,
    dirty: Arc<AtomicBool>,
    scanning: Arc<AtomicBool>,
    /// The index holds a first walk that has not finished yet.
    partial: Arc<AtomicBool>,
    rewalk_cancel: Arc<AtomicBool>,
    rewalk_coord: Arc<(Mutex<RewalkCoord>, Condvar)>,
    watcher: Option<VolumeWatcher>,
    rewalk_thread: Option<JoinHandle<()>>,
}

/// Multi-volume filesystem index, watcher, snapshot, and search service.
pub struct Service {
    data_dir: PathBuf,
    volumes: Vec<VolumeState>,
    housekeep_stop: Arc<(Mutex<bool>, Condvar)>,
    housekeep_thread: Option<JoinHandle<()>>,
    stopped: bool,
}

/// Returns all fixed local NTFS and ReFS volume roots (e.g. `["C:\\", "D:\\"]`).
#[must_use]
pub fn default_roots() -> Vec<String> {
    let mut needed = unsafe { GetLogicalDriveStringsW(None) };
    if needed == 0 {
        return Vec::new();
    }
    let mut buf = vec![0u16; needed as usize + 2];
    let mut written = 0u32;
    for _ in 0..4 {
        let len = unsafe { GetLogicalDriveStringsW(Some(&mut buf)) };
        if len == 0 {
            return Vec::new();
        }
        if (len as usize) <= buf.len() {
            written = len;
            break;
        }
        needed = len;
        buf.resize(needed as usize + 2, 0);
    }
    if written == 0 {
        return Vec::new();
    }

    let mut roots = Vec::new();
    let slice = &buf[..(written as usize).min(buf.len())];
    for part in slice.split(|&c| c == 0) {
        if part.is_empty() {
            continue;
        }
        let mut wide_drive = Vec::with_capacity(part.len() + 1);
        wide_drive.extend_from_slice(part);
        wide_drive.push(0);

        let dtype = unsafe { GetDriveTypeW(PCWSTR(wide_drive.as_ptr())) };
        if dtype != DRIVE_FIXED {
            continue;
        }

        let mut fs_buf = [0u16; 64];
        let vol_ok = unsafe {
            GetVolumeInformationW(
                PCWSTR(wide_drive.as_ptr()),
                None,
                None,
                None,
                None,
                Some(&mut fs_buf),
            )
        };
        if vol_ok.is_err() {
            continue;
        }
        let fs_len = fs_buf.iter().position(|&c| c == 0).unwrap_or(fs_buf.len());
        let fs_name = String::from_utf16_lossy(&fs_buf[..fs_len]);
        if fs_name.eq_ignore_ascii_case("NTFS") || fs_name.eq_ignore_ascii_case("ReFS") {
            roots.push(String::from_utf16_lossy(part));
        }
    }
    roots
}

fn cleanup_stale_temp_snapshots(data_dir: &Path) {
    let ext_dir = to_extended_path(&data_dir.to_string_lossy());
    let Ok(entries) = fs::read_dir(&ext_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let name = fname.to_string_lossy();
        if name.starts_with("index-") && name.contains(".bin.tmp.") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

impl Service {
    /// Starts the indexing service for `roots`, loading valid snapshots from `data_dir`
    /// for instant search and launching background watchers and re-walks.
    pub fn start(
        roots: Vec<String>,
        data_dir: PathBuf,
        on_event: Box<dyn Fn(Event) + Send + Sync>,
    ) -> Service {
        Self::start_with_watcher_config(roots, data_dir, WatcherConfig::default(), on_event)
    }

    /// Starts the service with a custom [`WatcherConfig`] (used in tests for forced overflow).
    pub fn start_with_watcher_config(
        roots: Vec<String>,
        data_dir: PathBuf,
        watcher_config: WatcherConfig,
        on_event: Box<dyn Fn(Event) + Send + Sync>,
    ) -> Service {
        cleanup_stale_temp_snapshots(&data_dir);

        let on_event: Arc<dyn Fn(Event) + Send + Sync> = Arc::from(on_event);
        let mut volumes = Vec::with_capacity(roots.len());

        for raw_root in roots {
            let root = normalize_display_path(&raw_root);
            let use_file_id = is_ntfs_volume(&root);
            let volume_serial = resolve_volume_serial(&root);
            let snap_path = snapshot_path(&data_dir, volume_serial);
            let loaded = load_snapshot_for_root(&snap_path, volume_serial, &root);

            let index = Arc::new(RwLock::new(loaded));
            let apply_mu = Arc::new(Mutex::new(()));
            let denied_filter = Arc::new(RwLock::new(DeniedFilter::default()));
            let dirty = Arc::new(AtomicBool::new(false));
            let scanning = Arc::new(AtomicBool::new(true));
            let partial = Arc::new(AtomicBool::new(false));
            let rewalk_cancel = Arc::new(AtomicBool::new(false));
            let rewalk_coord = Arc::new((
                Mutex::new(RewalkCoord {
                    running: false,
                    manual_or_timer_pending: true,
                    overflow_pending: false,
                    last_finished: None,
                    stop: false,
                    collecting: false,
                    during_walk: Vec::new(),
                    overflowed_during_walk: false,
                    failed_streak: 0,
                }),
                Condvar::new(),
            ));

            let watcher = {
                let coord_locked = Arc::clone(&rewalk_coord);
                let dirty_applied = Arc::clone(&dirty);
                let ev_applied = Arc::clone(&on_event);
                let coord_overflow = Arc::clone(&rewalk_coord);
                let coord_restarted = Arc::clone(&rewalk_coord);
                let ev_err = Arc::clone(&on_event);
                let root_err = root.clone();

                VolumeWatcher::start_resilient(
                    root.clone(),
                    Arc::clone(&index),
                    Arc::clone(&apply_mu),
                    Arc::clone(&denied_filter),
                    watcher_config,
                    use_file_id,
                    Box::new(move |records| capture_during_walk(&coord_locked.0, records)),
                    Box::new(move |_records, changed| {
                        if changed {
                            dirty_applied.store(true, Ordering::Release);
                            ev_applied(Event::Changed);
                        }
                    }),
                    Box::new(move || note_watcher_gap(&coord_overflow)),
                    // A restart is a gap too; routing it through the cooldown keeps a flapping
                    // watcher from forcing back-to-back full re-walks.
                    Box::new(move || note_watcher_restarted(&coord_restarted)),
                    Box::new(move |msg| {
                        ev_err(Event::Error {
                            root: root_err.clone(),
                            message: msg.to_owned(),
                        });
                    }),
                )
            };

            let rewalk_thread = {
                let root_w = root.clone();
                let index_w = Arc::clone(&index);
                let apply_w = Arc::clone(&apply_mu);
                let denied_w = Arc::clone(&denied_filter);
                let dirty_w = Arc::clone(&dirty);
                let scanning_w = Arc::clone(&scanning);
                let partial_w = Arc::clone(&partial);
                let cancel_w = Arc::clone(&rewalk_cancel);
                let coord_w = Arc::clone(&rewalk_coord);
                let ev_w = Arc::clone(&on_event);
                let overflow_cd = watcher_config.overflow_rewalk_cooldown;

                thread::Builder::new()
                    .name(format!("fm-rewalk:{root_w}"))
                    .spawn(move || {
                        rewalk_worker(
                            &Win32DirSource,
                            &root_w,
                            index_w,
                            apply_w,
                            denied_w,
                            use_file_id,
                            dirty_w,
                            scanning_w,
                            partial_w,
                            cancel_w,
                            coord_w,
                            overflow_cd,
                            ev_w,
                        );
                    })
                    .ok()
            };

            if rewalk_thread.is_none() {
                scanning.store(false, Ordering::Release);
            }

            volumes.push(VolumeState {
                volume_serial,
                index,
                dirty,
                scanning,
                partial,
                rewalk_cancel,
                rewalk_coord,
                watcher,
                rewalk_thread,
            });
        }

        let housekeep_stop = Arc::new((Mutex::new(false), Condvar::new()));
        let hk_stop = Arc::clone(&housekeep_stop);
        let hk_data_dir = data_dir.clone();
        let hk_vols: Vec<HousekeepTarget> = volumes
            .iter()
            .map(|v| HousekeepTarget {
                volume_serial: v.volume_serial,
                index: Arc::clone(&v.index),
                dirty: Arc::clone(&v.dirty),
                rewalk_coord: Arc::clone(&v.rewalk_coord),
            })
            .collect();

        let housekeep_thread = thread::Builder::new()
            .name("fm-service-housekeep".to_owned())
            .spawn(move || housekeep_loop(hk_data_dir, hk_vols, hk_stop))
            .ok();

        Service {
            data_dir,
            volumes,
            housekeep_stop,
            housekeep_thread,
            stopped: false,
        }
    }

    /// Executes a ranked query across all managed volumes, optionally scoped to `scope`.
    ///
    /// Returns `None` if cancelled via `cancel`.
    #[must_use]
    pub fn search(
        &self,
        query: &str,
        scope: Option<&str>,
        include_hidden: bool,
        limit: usize,
        cancel: &CancelToken,
    ) -> Option<Vec<Hit>> {
        if cancel.is_cancelled() {
            return None;
        }
        if limit == 0 {
            return Some(Vec::new());
        }
        let parsed = parse_query(query);
        if parsed.is_empty() || parsed.impossible {
            return Some(Vec::new());
        }

        let opts = SearchOptions {
            include_hidden,
            limit,
        };

        struct RankedHit {
            hit: Hit,
            tier: u8,
            name_chars: u16,
        }

        let mut merged: Vec<RankedHit> = Vec::new();
        for vol in &self.volumes {
            if cancel.is_cancelled() {
                return None;
            }
            let guard = read_or_recover(&vol.index);
            let scoped = guard.search_scoped(&parsed, scope, &opts, cancel)?;
            for sh in scoped {
                let meta = guard.meta(sh.id);
                let name = guard.name(sh.id).to_owned();
                let path = guard.path(sh.id);
                let is_dir = (meta.attrs & DIRECTORY) != 0;
                merged.push(RankedHit {
                    hit: Hit {
                        path,
                        name,
                        is_dir,
                        size: meta.size,
                        mtime: meta.mtime,
                        attrs: meta.attrs,
                    },
                    tier: sh.tier,
                    name_chars: sh.name_chars,
                });
            }
        }

        if cancel.is_cancelled() {
            return None;
        }

        if self.volumes.len() > 1 && merged.len() > 1 {
            let cmp = |a: &RankedHit, b: &RankedHit| {
                a.tier
                    .cmp(&b.tier)
                    .then_with(|| a.name_chars.cmp(&b.name_chars))
                    .then_with(|| b.hit.mtime.cmp(&a.hit.mtime))
                    .then_with(|| a.hit.path.cmp(&b.hit.path))
            };
            if merged.len() > limit {
                merged.select_nth_unstable_by(limit, cmp);
                merged.truncate(limit);
            }
            merged.sort_unstable_by(cmp);
        }

        Some(merged.into_iter().map(|r| r.hit).collect())
    }

    /// Sizes of the folders at `paths`: bytes of every file below each, as far as the index has
    /// seen them (`None` where no managed volume indexes the path). A folder it never walked
    /// (a junction, access denied) counts 0. `None` if cancelled. The read lock is taken per
    /// folder, so the watcher is never held up for a whole batch.
    #[must_use]
    pub fn folder_sizes(&self, paths: &[String], cancel: &CancelToken) -> Option<Vec<Option<u64>>> {
        let mut out = vec![None; paths.len()];
        for (k, p) in paths.iter().enumerate() {
            for vol in &self.volumes {
                if cancel.is_cancelled() {
                    return None;
                }
                // Mid first walk the sums are partial; they would show as real sizes.
                if vol.partial.load(Ordering::Acquire) {
                    continue;
                }
                let guard = read_or_recover(&vol.index);
                if let Some(id) = guard.lookup(p).filter(|&id| guard.is_dir(id)) {
                    out[k] = Some(guard.subtree_size(id).0);
                    break;
                }
            }
        }
        Some(out)
    }

    /// Returns the total live entry count across all managed volumes.
    #[must_use]
    pub fn entries(&self) -> usize {
        self.volumes
            .iter()
            .map(|v| read_or_recover(&v.index).len())
            .sum()
    }

    /// Returns `true` if any volume is currently executing an initial scan or background re-walk.
    #[must_use]
    pub fn is_scanning(&self) -> bool {
        self.volumes
            .iter()
            .any(|v| v.scanning.load(Ordering::Acquire))
    }

    /// Atomically saves snapshots for all volumes into `data_dir` using
    /// temp-file write + `FlushFileBuffers` + `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`.
    pub fn save(&self) {
        if fs::create_dir_all(&self.data_dir).is_err() {
            return;
        }
        for vol in &self.volumes {
            let _ = save_volume_snapshot(&self.data_dir, vol.volume_serial, &vol.index, &vol.dirty);
        }
    }

    /// Triggers an immediate background re-walk for every managed volume (bypassing overflow cooldown).
    pub fn trigger_rewalk(&self) {
        for vol in &self.volumes {
            let (mu, cv) = &*vol.rewalk_coord;
            let mut st = lock_or_recover(mu);
            if !st.stop {
                st.manual_or_timer_pending = true;
                st.failed_streak = 0;
                cv.notify_one();
            }
        }
    }

    /// Stops all background threads cleanly and saves dirty/missing snapshots.
    pub fn stop(mut self) {
        self.stop_threads();
        self.save_dirty_or_missing();
    }

    fn stop_threads(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;

        {
            let (mu, cv) = &*self.housekeep_stop;
            let mut stop = lock_or_recover(mu);
            *stop = true;
            cv.notify_all();
        }
        if let Some(h) = self.housekeep_thread.take() {
            let _ = h.join();
        }

        for vol in &mut self.volumes {
            vol.rewalk_cancel.store(true, Ordering::Release);
            if let Some(w) = vol.watcher.take() {
                w.stop();
            }
            {
                let (mu, cv) = &*vol.rewalk_coord;
                let mut st = lock_or_recover(mu);
                st.stop = true;
                cv.notify_all();
            }
        }
        for vol in &mut self.volumes {
            if let Some(h) = vol.rewalk_thread.take() {
                let _ = h.join();
            }
        }
    }

    fn save_dirty_or_missing(&self) {
        if fs::create_dir_all(&self.data_dir).is_err() {
            return;
        }
        for vol in &self.volumes {
            let path = snapshot_path(&self.data_dir, vol.volume_serial);
            let ext = to_extended_path(&path.to_string_lossy());
            if !vol.dirty.load(Ordering::Acquire) && Path::new(&ext).exists() {
                continue;
            }
            let _ = save_volume_snapshot(&self.data_dir, vol.volume_serial, &vol.index, &vol.dirty);
        }
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.stop_threads();
    }
}

struct HousekeepTarget {
    volume_serial: u32,
    index: Arc<RwLock<Index>>,
    dirty: Arc<AtomicBool>,
    rewalk_coord: Arc<(Mutex<RewalkCoord>, Condvar)>,
}

fn housekeep_loop(
    data_dir: PathBuf,
    targets: Vec<HousekeepTarget>,
    stop_sync: Arc<(Mutex<bool>, Condvar)>,
) {
    let mut last_save = Instant::now();
    let mut last_rewalk = Instant::now();
    let (mu, cv) = &*stop_sync;

    loop {
        {
            let guard = lock_or_recover(mu);
            if *guard {
                break;
            }
            let (next_guard, _) = wait_timeout_or_recover(cv, guard, HOUSEKEEP_TICK);
            if *next_guard {
                break;
            }
        }

        let now = Instant::now();
        if now.duration_since(last_save) >= HOURLY_SAVE_INTERVAL {
            if fs::create_dir_all(&data_dir).is_ok() {
                for t in &targets {
                    if t.dirty.load(Ordering::Acquire) {
                        let _ =
                            save_volume_snapshot(&data_dir, t.volume_serial, &t.index, &t.dirty);
                    }
                }
            }
            last_save = now;
        }

        if now.duration_since(last_rewalk) >= DAILY_REWALK_INTERVAL {
            for t in &targets {
                let (rmu, rcv) = &*t.rewalk_coord;
                let mut st = lock_or_recover(rmu);
                if !st.stop {
                    st.manual_or_timer_pending = true;
                    rcv.notify_one();
                }
            }
            last_rewalk = now;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn rewalk_worker<S: DirSource>(
    source: &S,
    root: &str,
    index: Arc<RwLock<Index>>,
    apply_mu: Arc<Mutex<()>>,
    denied_filter: Arc<RwLock<DeniedFilter>>,
    use_file_id: bool,
    dirty: Arc<AtomicBool>,
    scanning: Arc<AtomicBool>,
    partial: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    coord: Arc<(Mutex<RewalkCoord>, Condvar)>,
    overflow_cooldown: Duration,
    on_event: Arc<dyn Fn(Event) + Send + Sync>,
) where
    S::Handle: Send,
{
    let (mu, cv) = &*coord;
    let fs = Win32WatcherFs;
    let mut emitted_initial_ready = false;

    loop {
        {
            let mut st = lock_or_recover(mu);
            loop {
                if st.stop {
                    st.running = false;
                    st.collecting = false;
                    scanning.store(false, Ordering::Release);
                    return;
                }
                if st.manual_or_timer_pending {
                    st.manual_or_timer_pending = false;
                    st.overflow_pending = false;
                    break;
                }
                if st.overflow_pending {
                    let cooldown = if st.failed_streak > 0 {
                        failed_walk_cooldown(overflow_cooldown, st.failed_streak)
                    } else {
                        overflow_cooldown
                    };
                    if let Some(last) = st.last_finished
                        && !cooldown.is_zero()
                    {
                        let ready_at = last + cooldown;
                        let now = Instant::now();
                        if now < ready_at {
                            scanning.store(false, Ordering::Release);
                            let wait_dur = ready_at.saturating_duration_since(now);
                            let (next_st, _) = wait_timeout_or_recover(cv, st, wait_dur);
                            st = next_st;
                            continue;
                        }
                    }
                    st.overflow_pending = false;
                    break;
                }
                scanning.store(false, Ordering::Release);
                st = wait_or_recover(cv, st);
            }
            st.running = true;
            st.collecting = true;
            st.during_walk.clear();
            st.overflowed_during_walk = false;
            scanning.store(true, Ordering::Release);
        }

        let walk_opts = WalkOptions {
            low_priority: true,
            ..Default::default()
        };
        let walk_started = Instant::now();
        // Nothing indexed yet (no snapshot): show results while the walk runs. A re-walk keeps
        // the existing index untouched until its merge.
        let progressive = read_or_recover(&index).is_empty();
        if progressive {
            partial.store(true, Ordering::Release);
        }
        let batches = Mutex::new(Vec::new());
        let sink = |b: WalkBatch| lock_or_recover(&batches).push(b);
        let mut last_insert: Option<Instant> = None;
        let walk_res = walk_with_source_streaming(
            source,
            root,
            walk_opts,
            &cancel,
            |entries| {
                on_event(Event::Progress {
                    root: root.to_owned(),
                    entries,
                });
                if progressive && last_insert.is_none_or(|t| t.elapsed() >= PROGRESSIVE_INTERVAL)
                {
                    last_insert = Some(Instant::now());
                    let listed = std::mem::take(&mut *lock_or_recover(&batches));
                    if insert_walk_batches(root, &index, &apply_mu, &denied_filter, listed) {
                        on_event(Event::Changed);
                    }
                }
            },
            progressive.then_some(&sink as &(dyn Fn(WalkBatch) + Sync)),
        );
        drop(batches);

        let Some((fresh, stats, walk_report)) = walk_res else {
            let mut st = lock_or_recover(mu);
            st.running = false;
            st.collecting = false;
            st.during_walk.clear();
            scanning.store(false, Ordering::Release);
            if st.stop || cancel.load(Ordering::Acquire) {
                break;
            }
            // Not cancelled: a walker worker panicked. Keep live as is and retry later.
            st.last_finished = Some(Instant::now());
            st.failed_streak = st.failed_streak.saturating_add(1);
            st.overflow_pending = true;
            let first_failure = st.failed_streak == 1;
            drop(st);
            if first_failure {
                on_event(Event::Error {
                    root: root.to_owned(),
                    message: format!("walk of {root} failed; keeping the existing index"),
                });
            }
            emitted_initial_ready = true;
            on_event(Event::Ready {
                root: root.to_owned(),
                entries: read_or_recover(&index).len(),
                millis: walk_started.elapsed().as_millis() as u64,
            });
            continue;
        };

        let (changed, final_entries) = apply_rewalk_result(
            root,
            &index,
            &apply_mu,
            &denied_filter,
            mu,
            fresh,
            &walk_report,
            use_file_id,
            progressive,
            &fs,
            &cancel,
        );
        partial.store(false, Ordering::Release);

        if changed {
            dirty.store(true, Ordering::Release);
        }

        let next_manual_immediate = {
            let mut st = lock_or_recover(mu);
            st.running = false;
            st.last_finished = Some(Instant::now());
            st.failed_streak = if walk_report.root_failed {
                st.failed_streak.saturating_add(1)
            } else {
                0
            };
            let immediate = st.manual_or_timer_pending && !st.stop;
            if !immediate {
                scanning.store(false, Ordering::Release);
            }
            immediate
        };

        if emitted_initial_ready && changed {
            on_event(Event::Changed);
        }
        emitted_initial_ready = true;
        on_event(Event::Ready {
            root: root.to_owned(),
            entries: final_entries,
            millis: stats.millis,
        });
        let _ = next_manual_immediate;
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_rewalk_result(
    root: &str,
    index: &Arc<RwLock<Index>>,
    apply_mu: &Arc<Mutex<()>>,
    denied_filter: &Arc<RwLock<DeniedFilter>>,
    coord_mu: &Mutex<RewalkCoord>,
    fresh: Index,
    walk_report: &WalkFoldersReport,
    use_file_id: bool,
    progressive: bool,
    fs: &dyn crate::win::watcher::WatcherFs,
    cancel: &AtomicBool,
) -> (bool, usize) {
    let _apply_guard = lock_or_recover(apply_mu);

    let (replay, mut changed, needs_prefetch, parent_hints, skip_list, mut denied_snap, live_len) = {
        let mut live = write_or_recover(index);
        let replay = {
            let mut st = lock_or_recover(coord_mu);
            st.collecting = false;
            if st.overflowed_during_walk || walk_report.root_failed {
                st.overflow_pending = true;
            }
            std::mem::take(&mut st.during_walk)
        };

        if walk_report.root_failed {
            // `live` was not merged and the watcher already applied every captured record to
            // it; replaying them again is not idempotent. Keep live and the denied filter.
            return (false, live.len());
        }
        if progressive {
            // `live` holds only what this walk showed while it ran (plus watcher edits, which
            // the replay re-applies): `fresh` replaces it as on any first walk, tightly sized.
            *live = Index::new();
        }

        let active_denied = DeniedFilter::from_walk(&walk_report.denied);
        *write_or_recover(denied_filter) = active_denied.clone();
        let mut ch = merge_index_with_report(&mut live, fresh, walk_report, use_file_id);

        if replay.is_empty() {
            return (ch, live.len());
        } else if !batch_needs_prefetch(&replay) {
            if let Some(root_id) = live.lookup(root) {
                ch |= replay_records_filtered(
                    &mut live,
                    root_id,
                    &replay,
                    &NoopWatcherFs,
                    &active_denied,
                    use_file_id,
                );
            }
            return (ch, live.len());
        }
        let (hints, skip) = match live.lookup(root) {
            Some(rid) => (
                if use_file_id {
                    collect_parent_hints(&live, rid, &replay)
                } else {
                    HashMap::new()
                },
                renames_resolved_in_index(&live, rid, &replay, None, &active_denied, use_file_id),
            ),
            None => (HashMap::new(), HashSet::new()),
        };
        (replay, ch, true, hints, skip, active_denied, live.len())
    };

    if !needs_prefetch || replay.is_empty() {
        return (changed, live_len);
    }

    // List Added directory subtrees and resolve 8.3 short names strictly outside index.write().
    let prefetched = prefetch_batch_outside_lock(
        root,
        &replay,
        &parent_hints,
        &skip_list,
        &denied_snap,
        use_file_id,
        fs,
        cancel,
    );
    if !prefetched.extra_denied.is_empty() {
        denied_snap.extend_from(&prefetched.extra_denied);
        write_or_recover(denied_filter).extend_from(&prefetched.extra_denied);
    }

    let mut live = write_or_recover(index);
    if let Some(root_id) = live.lookup(root) {
        changed |= replay_records_filtered(
            &mut live,
            root_id,
            &replay,
            &prefetched,
            &denied_snap,
            use_file_id,
        );
    }
    (changed, live.len())
}

/// Puts a first walk's listings into the live index while the walk runs (the finished walk then
/// replaces them). Each folder appears with its own listing, so one that failed before listing
/// anything never shows, as after the walk. Entries are found by path because the watcher may
/// compact `live` meanwhile, renumbering every `EntryId`. The volume serial stays unset until the
/// walk ends, so a snapshot saved from a cancelled walk fails to load (on volumes with a serial).
/// Returns `true` if anything was inserted.
fn insert_walk_batches(
    root: &str,
    index: &RwLock<Index>,
    apply_mu: &Mutex<()>,
    denied_filter: &RwLock<DeniedFilter>,
    batches: Vec<WalkBatch>,
) -> bool {
    if batches.is_empty() {
        return false;
    }
    let _apply_guard = lock_or_recover(apply_mu);
    let mut live = write_or_recover(index);
    let mut denied = Vec::new();
    let mut changed = false;
    for b in batches {
        // A root that lists nothing is root_failed or empty: the end of the walk decides both.
        if b.children.is_empty() && (b.failed || b.rel_path.is_empty()) {
            continue;
        }
        let id = if b.rel_path.is_empty() {
            live.add_root(root)
        } else {
            let (parent_rel, name) = b.rel_path.rsplit_once('\\').unwrap_or(("", &b.rel_path));
            // Gone or renamed by the watcher meanwhile: the end of the walk settles it.
            let Some(parent) = live
                .lookup(&join_child_path(root, parent_rel))
                .filter(|&p| live.is_dir(p))
            else {
                continue;
            };
            live.insert(parent, name, b.meta)
        };
        for (name, meta, descend) in &b.children {
            if !descend {
                live.insert(id, name, *meta);
            }
        }
        if b.denied {
            denied.push(WalkFolderRef {
                file_id: b.meta.file_id,
                rel_path: b.rel_path,
            });
        }
        changed = true;
    }
    // The watcher already applies to these folders; drop its updates under denied ones now,
    // not only once the walk ends.
    if !denied.is_empty() {
        write_or_recover(denied_filter).extend_from(&denied);
    }
    changed
}

fn resolve_volume_serial(root: &str) -> u32 {
    let mut wbuf = Vec::new();
    if let Ok(h) = open_dir_handle(root, false, &mut wbuf)
        && let Ok((serial, _, _, _)) = query_dir_info_by_handle(h.0)
    {
        return serial;
    }
    let bytes = root.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let drive_root = format!("{}:\\", bytes[0] as char);
        let w = wide_null(&drive_root);
        let mut serial = 0u32;
        let ok = unsafe {
            GetVolumeInformationW(
                PCWSTR(w.as_ptr()),
                None,
                Some(&mut serial),
                None,
                None,
                None,
            )
        };
        if ok.is_ok() {
            return serial;
        }
    }
    0
}

fn snapshot_path(data_dir: &Path, volume_serial: u32) -> PathBuf {
    data_dir.join(format!("index-{volume_serial:08x}.bin"))
}

fn load_snapshot_for_root(path: &Path, volume_serial: u32, root: &str) -> Index {
    let ext_path = to_extended_path(&path.to_string_lossy());
    let Ok(file) = File::open(&ext_path) else {
        return Index::new();
    };
    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    let expected = if volume_serial != 0 {
        Some(volume_serial)
    } else {
        None
    };
    match Index::load(&mut reader, expected) {
        Ok(idx) if !idx.is_empty() && idx.lookup(root).is_some() => idx,
        _ => Index::new(),
    }
}

/// Saves `index` atomically. `dirty` is cleared while the read lock is held, before serialising,
/// so a change applied after the lock is released (during flush/rename) re-marks the volume dirty
/// instead of being wiped by a late clear; a failed save restores it.
fn save_volume_snapshot(
    data_dir: &Path,
    volume_serial: u32,
    index: &RwLock<Index>,
    dirty: &AtomicBool,
) -> io::Result<bool> {
    let dst_path = snapshot_path(data_dir, volume_serial);
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_path = data_dir.join(format!(
        "index-{volume_serial:08x}.bin.tmp.{}.{seq}",
        std::process::id()
    ));
    let tmp_ext = to_extended_path(&tmp_path.to_string_lossy());
    let dst_ext = to_extended_path(&dst_path.to_string_lossy());

    let mut was_dirty = false;
    let write_res: io::Result<bool> = (|| {
        let guard = read_or_recover(index);
        if guard.is_empty() {
            return Ok(false);
        }
        was_dirty = dirty.swap(false, Ordering::AcqRel);
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_ext)?;
        let mut bw = BufWriter::with_capacity(1024 * 1024, file);
        guard.save(&mut bw)?;
        bw.flush()?;
        // Release index read lock immediately after serialization + user-space flush so disk
        // FlushFileBuffers and MoveFileExW never block concurrent writers or watcher updates.
        drop(guard);

        let file = bw.into_inner().map_err(io::IntoInnerError::into_error)?;
        let handle = HANDLE(file.as_raw_handle());
        unsafe { FlushFileBuffers(handle) }.map_err(win32_to_io)?;
        drop(file);

        let tmp_w = wide_null(&tmp_ext);
        let dst_w = wide_null(&dst_ext);
        unsafe {
            MoveFileExW(
                PCWSTR(tmp_w.as_ptr()),
                PCWSTR(dst_w.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(win32_to_io)?;
        Ok(true)
    })();

    if !matches!(write_res, Ok(true)) {
        if was_dirty {
            dirty.store(true, Ordering::Release);
        }
        let _ = fs::remove_file(&tmp_ext);
    }
    write_res
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use fm_fs::attrs::DIRECTORY;

    use super::*;
    use crate::Meta;
    use crate::win::walker::{DirEntryRef, RootInfo, walk_with_source_report};
    use crate::win::watcher::NotifyAction;

    #[test]
    fn root_failed_rewalk_preserves_denied_filter_and_live_index() {
        let mut live_idx = Index::new();
        let r = live_idx.add_root("C:\\Temp");
        live_idx.set_file_id(r, 1);
        live_idx.insert(
            r,
            "Secret",
            Meta {
                attrs: DIRECTORY,
                size: 0,
                mtime: 10,
                file_id: 99,
            },
        );

        let index = Arc::new(RwLock::new(live_idx));
        let apply_mu = Arc::new(Mutex::new(()));
        let initial_denied = DeniedFilter::from_walk(&[WalkFolderRef {
            file_id: 99,
            rel_path: "Secret".to_owned(),
        }]);
        let denied_filter = Arc::new(RwLock::new(initial_denied));
        let coord = Mutex::new(RewalkCoord {
            running: true,
            manual_or_timer_pending: false,
            overflow_pending: false,
            last_finished: None,
            stop: false,
            collecting: true,
            during_walk: Vec::new(),
            overflowed_during_walk: false,
            failed_streak: 0,
        });

        let failed_report = WalkFoldersReport {
            root_failed: true,
            failed: vec![WalkFolderRef {
                file_id: 0,
                rel_path: String::new(),
            }],
            denied: Vec::new(),
        };

        let (changed, entries) = apply_rewalk_result(
            "C:\\Temp",
            &index,
            &apply_mu,
            &denied_filter,
            &coord,
            Index::new(),
            &failed_report,
            true,
            false,
            &NoopWatcherFs,
            &AtomicBool::new(false),
        );
        assert!(!changed);
        assert_eq!(entries, 2);

        // Pre-existing DeniedFilter must NOT have been wiped by the transient root_failed walk
        let rec = NotifyRecord {
            action: NotifyAction::Added,
            rel_path: "Secret\\leak.txt".to_owned(),
            file_id: 500,
            parent_file_id: 99,
            attrs: fm_fs::attrs::ARCHIVE,
            size: 1,
            mtime: 1,
            reparse_tag: 0,
        };
        let df = read_or_recover(&denied_filter);
        assert!(df.is_record_denied(None, &rec, true));
    }

    const ARC: u32 = fm_fs::attrs::ARCHIVE;

    fn rec(action: NotifyAction, rel: &str, fid: u64, pfid: u64, attrs: u32) -> NotifyRecord {
        NotifyRecord {
            action,
            rel_path: rel.to_owned(),
            file_id: fid,
            parent_file_id: pfid,
            attrs,
            size: 0,
            mtime: 1,
            reparse_tag: 0,
        }
    }

    fn meta(attrs: u32, fid: u64) -> Meta {
        Meta {
            attrs,
            size: 0,
            mtime: 1,
            file_id: fid,
        }
    }

    fn coord(collecting: bool) -> (Mutex<RewalkCoord>, Condvar) {
        (
            Mutex::new(RewalkCoord {
                running: collecting,
                manual_or_timer_pending: false,
                overflow_pending: false,
                last_finished: None,
                stop: false,
                collecting,
                during_walk: Vec::new(),
                overflowed_during_walk: false,
                failed_streak: 0,
            }),
            Condvar::new(),
        )
    }

    /// Runs the service's merge + replay step for one walk result.
    fn rewalk(
        live: Index,
        fresh: Index,
        report: &WalkFoldersReport,
        co: &(Mutex<RewalkCoord>, Condvar),
        use_file_id: bool,
    ) -> (Index, bool) {
        let index = Arc::new(RwLock::new(live));
        let (changed, _) = apply_rewalk_result(
            "R:\\",
            &index,
            &Arc::new(Mutex::new(())),
            &Arc::new(RwLock::new(DeniedFilter::default())),
            &co.0,
            fresh,
            report,
            use_file_id,
            false,
            &NoopWatcherFs,
            &AtomicBool::new(false),
        );
        let idx = std::mem::take(&mut *index.write().unwrap());
        (idx, changed)
    }

    fn renamed_a2_index() -> Index {
        let mut idx = Index::new();
        let r = idx.add_root("R:\\");
        idx.set_file_id(r, 1);
        let a2 = idx.insert(r, "A2", meta(DIRECTORY, 7));
        idx.insert(a2, "f.txt", meta(ARC, 8));
        idx
    }

    #[test]
    fn replay_stops_at_watcher_gap_and_drops_orphan_old_name() {
        // Fails on 4056029: records were captured across the overflow gap, so the replay paired
        // the orphan `RenamedOldName A` with the unrelated `Added b.txt` (or removed it as a
        // trailing orphan) and its FileId fallback deleted the renamed `A2` with its child.
        for post_gap in [true, false] {
            let co = coord(true);
            capture_during_walk(
                &co.0,
                &[rec(NotifyAction::RenamedOldName, "A", 7, 1, DIRECTORY)],
            );
            note_watcher_gap(&co);
            if post_gap {
                capture_during_walk(&co.0, &[rec(NotifyAction::Added, "b.txt", 9, 1, ARC)]);
            }
            assert_eq!(co.0.lock().unwrap().during_walk.len(), 1);
            let (idx, _) = rewalk(
                renamed_a2_index(),
                renamed_a2_index(),
                &WalkFoldersReport::default(),
                &co,
                true,
            );
            assert!(idx.lookup("R:\\A2\\f.txt").is_some(), "post_gap={post_gap}");
            let st = co.0.lock().unwrap();
            assert!(st.overflow_pending && st.during_walk.is_empty());
        }
    }

    #[test]
    fn root_failed_rewalk_skips_replay_and_schedules_retry() {
        // Fails on 4056029: the captured records (already applied to live by the watcher) were
        // replayed onto the unmerged live index, deleting the 3 children, and no retry was set.
        let mut live = Index::new();
        let r = live.add_root("R:\\");
        live.set_file_id(r, 1);
        let p = live.insert(r, "Photos 2026", meta(DIRECTORY, 7));
        for i in 0..3u64 {
            live.insert(p, &format!("img{i}.jpg"), meta(ARC, 100 + i));
        }
        let report = WalkFoldersReport {
            root_failed: true,
            failed: vec![WalkFolderRef {
                file_id: 0,
                rel_path: String::new(),
            }],
            denied: Vec::new(),
        };
        for use_file_id in [true, false] {
            let co = coord(true);
            capture_during_walk(
                &co.0,
                &[
                    rec(NotifyAction::Added, "Album", 7, 1, DIRECTORY),
                    rec(NotifyAction::RenamedOldName, "Album", 7, 1, DIRECTORY),
                    rec(NotifyAction::RenamedNewName, "Photos 2026", 7, 1, DIRECTORY),
                ],
            );
            let (idx, changed) = rewalk(live.clone(), Index::new(), &report, &co, use_file_id);
            assert!(!changed);
            let kids = (0..3)
                .filter(|i| {
                    idx.lookup(&format!("R:\\Photos 2026\\img{i}.jpg"))
                        .is_some()
                })
                .count();
            assert_eq!(kids, 3, "use_file_id={use_file_id}");
            let st = co.0.lock().unwrap();
            assert!(st.overflow_pending, "root_failed must schedule a retry");
            assert!(st.during_walk.is_empty() && !st.collecting);
        }
    }

    #[test]
    fn replay_onto_fresh_walk_is_idempotent_for_add_then_rename() {
        // Fails on 4056029 (pre-existing): fresh already lists `Photos 2026` with 3 children; the
        // replayed `Added Album` + rename deleted them (3 -> 0).
        for use_file_id in [true, false] {
            let mut fresh = Index::new();
            let r = fresh.add_root("R:\\");
            fresh.set_file_id(r, 1);
            let p = fresh.insert(r, "Photos 2026", meta(DIRECTORY, 7));
            for i in 0..3u64 {
                fresh.insert(p, &format!("img{i}.jpg"), meta(ARC, 100 + i));
            }
            let mut live = Index::new();
            let lr = live.add_root("R:\\");
            live.set_file_id(lr, 1);
            let co = coord(true);
            capture_during_walk(
                &co.0,
                &[
                    rec(NotifyAction::Added, "Album", 7, 1, DIRECTORY),
                    rec(NotifyAction::RenamedOldName, "Album", 7, 1, DIRECTORY),
                    rec(NotifyAction::RenamedNewName, "Photos 2026", 7, 1, DIRECTORY),
                ],
            );
            let (idx, _) = rewalk(live, fresh, &WalkFoldersReport::default(), &co, use_file_id);
            let kids = (0..3)
                .filter(|i| {
                    idx.lookup(&format!("R:\\Photos 2026\\img{i}.jpg"))
                        .is_some()
                })
                .count();
            assert_eq!(kids, 3, "use_file_id={use_file_id}");
            assert_eq!(idx.lookup("R:\\Album"), None);
        }
    }

    #[test]
    fn rewalk_production_state_rename_during_walk_keeps_subtree() {
        // Production state of defect 1: the watcher already renamed P -> Q in live, the walker
        // listed D (seeing P) and then failed to open P; fresh holds an empty P stub.
        // Fails on 4056029 for path-only volumes: the merge deleted Q's subtree.
        let live = || {
            let mut idx = Index::new();
            let r = idx.add_root("R:\\");
            idx.set_file_id(r, 1);
            let d = idx.insert(r, "D", meta(DIRECTORY, 2));
            let q = idx.insert(d, "Q", meta(DIRECTORY, 10));
            idx.insert(q, "c1.txt", meta(ARC, 11));
            idx.insert(q, "c2.txt", meta(ARC, 12));
            idx
        };
        let fresh = || {
            let mut idx = Index::new();
            let r = idx.add_root("R:\\");
            idx.set_file_id(r, 1);
            let d = idx.insert(r, "D", meta(DIRECTORY, 2));
            idx.insert(d, "P", meta(DIRECTORY, 10));
            idx
        };
        let report = WalkFoldersReport {
            root_failed: false,
            failed: vec![WalkFolderRef {
                file_id: 10,
                rel_path: "D\\P".to_owned(),
            }],
            denied: Vec::new(),
        };
        for use_file_id in [true, false] {
            let mut merged = live();
            merge_index_with_report(&mut merged, fresh(), &report, use_file_id);
            let kept = merged.lookup("R:\\D\\Q\\c1.txt").is_some()
                || merged.lookup("R:\\D\\P\\c1.txt").is_some();
            assert!(
                kept,
                "merge must keep the subtree, use_file_id={use_file_id}"
            );

            let co = coord(true);
            capture_during_walk(
                &co.0,
                &[
                    rec(NotifyAction::RenamedOldName, "D\\P", 10, 2, DIRECTORY),
                    rec(NotifyAction::RenamedNewName, "D\\Q", 10, 2, DIRECTORY),
                ],
            );
            let (idx, _) = rewalk(live(), fresh(), &report, &co, use_file_id);
            assert!(idx.lookup("R:\\D\\Q\\c1.txt").is_some(), "{use_file_id}");
            assert!(idx.lookup("R:\\D\\Q\\c2.txt").is_some(), "{use_file_id}");
            assert_eq!(idx.lookup("R:\\D\\P"), None, "{use_file_id}");
        }
    }

    #[test]
    fn watcher_restart_goes_through_overflow_cooldown() {
        // Fails on 4056029: `on_restarted` set `manual_or_timer_pending`, bypassing the cooldown,
        // so a watcher that armed and failed again forced a full re-walk about every second.
        let co = coord(true);
        co.0.lock().unwrap().last_finished = Some(Instant::now());
        note_watcher_gap(&co);
        let st = co.0.lock().unwrap();
        assert!(!st.manual_or_timer_pending);
        assert!(st.overflow_pending && st.overflowed_during_walk);
        drop(st);

        // A restart means the root is back: the failed-walk backoff must not delay the walk.
        co.0.lock().unwrap().failed_streak = 7;
        note_watcher_restarted(&co);
        let st = co.0.lock().unwrap();
        assert!(!st.manual_or_timer_pending && st.overflow_pending);
        assert_eq!(st.failed_streak, 0);
    }

    #[test]
    fn save_keeps_dirty_set_by_a_change_made_while_saving() {
        // Fails on 4056029: `dirty` was cleared after FlushFileBuffers + MoveFileExW, wiping the
        // flag a concurrent change set after the read lock was released, so stop() skipped it.
        // The concurrent change is simulated by a writer racing the save (timing-dependent); it
        // exercises save_volume_snapshot itself, not 4056029's call sites.
        let data_dir = std::env::temp_dir().join(format!(
            "fm_idx_dirty_{}_{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&data_dir).unwrap();
        let mut idx = Index::new();
        let r = idx.add_root("R:\\");
        for i in 0..200_000u64 {
            idx.insert(r, &format!("file_{i}.txt"), meta(ARC, 10 + i));
        }
        let index = Arc::new(RwLock::new(idx));
        let dirty = Arc::new(AtomicBool::new(true));
        let save_done = Arc::new(AtomicBool::new(false));

        let writer = {
            let (index, dirty, save_done, dir) = (
                Arc::clone(&index),
                Arc::clone(&dirty),
                Arc::clone(&save_done),
                data_dir.clone(),
            );
            thread::spawn(move || {
                // Wait for serialisation to start, then queue a change behind the read lock.
                loop {
                    let tmp_seen = fs::read_dir(&dir)
                        .unwrap()
                        .flatten()
                        .any(|e| e.file_name().to_string_lossy().contains(".bin.tmp."));
                    if tmp_seen || save_done.load(Ordering::Acquire) {
                        break;
                    }
                    thread::yield_now();
                }
                let mut g = index.write().unwrap();
                let root = g.lookup("R:\\").unwrap();
                g.insert(root, "late.txt", meta(ARC, 1));
                drop(g);
                dirty.store(true, Ordering::Release);
            })
        };
        let res = save_volume_snapshot(&data_dir, 0xD1E7, &index, &dirty);
        save_done.store(true, Ordering::Release);
        writer.join().unwrap();
        assert!(matches!(res, Ok(true)));
        assert!(
            dirty.load(Ordering::Acquire),
            "a change made during the save must stay dirty"
        );

        // A failed save restores the flag.
        let missing = data_dir.join("no_such_dir");
        let clean = AtomicBool::new(true);
        assert!(save_volume_snapshot(&missing, 0xD1E7, &index, &clean).is_err());
        assert!(clean.load(Ordering::Acquire));
        let _ = fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn failing_root_walk_retries_with_growing_delay() {
        // Fails before this fix: a root that never opens was re-walked at a fixed
        // max(cooldown, FAILED_WALK_RETRY_MIN) forever (one Ready + scanning flip each time).
        let root = std::env::temp_dir()
            .join(format!("fm_idx_missing_root_{}", std::process::id()))
            .join("never_created");
        let root = root.to_string_lossy().into_owned();
        let coord = Arc::new(coord(false));
        coord.0.lock().unwrap().overflow_pending = true;
        let readys = Arc::new(Mutex::new(Vec::new()));
        let started = Instant::now();
        let worker = {
            let (root, coord, readys) = (root.clone(), Arc::clone(&coord), Arc::clone(&readys));
            thread::spawn(move || {
                rewalk_worker(
                    &Win32DirSource,
                    &root,
                    Arc::new(RwLock::new(Index::new())),
                    Arc::new(Mutex::new(())),
                    Arc::new(RwLock::new(DeniedFilter::default())),
                    true,
                    Arc::new(AtomicBool::new(false)),
                    Arc::new(AtomicBool::new(false)),
                    Arc::new(AtomicBool::new(false)),
                    Arc::new(AtomicBool::new(false)),
                    coord,
                    Duration::ZERO,
                    Arc::new(move |ev| {
                        if matches!(ev, Event::Ready { .. }) {
                            readys.lock().unwrap().push(started.elapsed());
                        }
                    }),
                )
            })
        };
        // Retries at ~0, 0.1, 0.3, 0.7, 1.5 s with doubling; ~16 with a fixed 100 ms delay.
        thread::sleep(Duration::from_millis(1600));
        {
            let mut st = coord.0.lock().unwrap();
            st.stop = true;
            coord.1.notify_all();
        }
        worker.join().unwrap();
        let n = readys.lock().unwrap().len();
        assert!((3..=6).contains(&n), "{n} failed-walk retries in 1.6 s");
        assert_eq!(
            failed_walk_cooldown(Duration::from_secs(60), 1),
            Duration::from_secs(60)
        );
        assert_eq!(
            failed_walk_cooldown(Duration::from_secs(60), 3),
            Duration::from_secs(240)
        );
        assert_eq!(
            failed_walk_cooldown(Duration::from_secs(60), 40),
            FAILED_WALK_RETRY_MAX
        );
    }

    #[test]
    fn rename_race_in_progressive_first_walk_leaves_one_folder() {
        // During a first walk the watcher already applies: it renames P -> Q (P not shown yet,
        // so it lands as an Added Q), then P's pre-rename listing is inserted. Live holds two
        // folders with FileId 10. When a walker worker panics, the retry merges that partial
        // index (a finished first walk replaces it instead): merge + replay must leave only Q.
        for q_listed in [false, true] {
            for use_file_id in [true, false] {
                let mut live = Index::new();
                let r = live.add_root("R:\\");
                live.set_file_id(r, 1);
                let d = live.insert(r, "D", meta(DIRECTORY, 2));
                let q = live.insert(d, "Q", meta(DIRECTORY, 10));
                let p = live.insert(d, "P", meta(DIRECTORY, 10));
                let mut fresh = Index::new();
                let fr = fresh.add_root("R:\\");
                fresh.set_file_id(fr, 1);
                let fd = fresh.insert(fr, "D", meta(DIRECTORY, 2));
                let fp = fresh.insert(fd, "P", meta(DIRECTORY, 10));
                for (fid, name) in [(11, "c1.txt"), (12, "c2.txt")] {
                    live.insert(p, name, meta(ARC, fid));
                    if q_listed {
                        live.insert(q, name, meta(ARC, fid));
                    }
                    fresh.insert(fp, name, meta(ARC, fid));
                }
                let co = coord(true);
                capture_during_walk(
                    &co.0,
                    &[
                        rec(NotifyAction::RenamedOldName, "D\\P", 10, 2, DIRECTORY),
                        rec(NotifyAction::RenamedNewName, "D\\Q", 10, 2, DIRECTORY),
                    ],
                );
                let (idx, _) = rewalk(live, fresh, &WalkFoldersReport::default(), &co, use_file_id);
                let tag = format!("q_listed={q_listed} use_file_id={use_file_id}");
                assert!(idx.lookup("R:\\D\\Q\\c1.txt").is_some(), "{tag}");
                assert!(idx.lookup("R:\\D\\Q\\c2.txt").is_some(), "{tag}");
                assert_eq!(idx.lookup("R:\\D\\P"), None, "{tag}");
                assert_eq!(idx.len(), 5, "{tag}");
            }
        }
    }

    /// Win32 listing where `gate` opens only once released and `gone` never opens (a folder
    /// that vanished after its parent listed it).
    struct GatedSource {
        gate: String,
        gone: String,
        open: (Mutex<bool>, Condvar),
    }

    impl GatedSource {
        fn release(&self) {
            *self.open.0.lock().unwrap() = true;
            self.open.1.notify_all();
        }
    }

    impl DirSource for GatedSource {
        type Handle = <Win32DirSource as DirSource>::Handle;
        type Scratch = <Win32DirSource as DirSource>::Scratch;

        fn open_dir(&self, path: &str) -> io::Result<Self::Handle> {
            if path == self.gone {
                return Err(io::ErrorKind::NotFound.into());
            }
            if path == self.gate {
                let mut open = self.open.0.lock().unwrap();
                while !*open {
                    open = self.open.1.wait(open).unwrap();
                }
            }
            Win32DirSource.open_dir(path)
        }

        fn root_info(&self, dir: &Self::Handle) -> io::Result<RootInfo> {
            Win32DirSource.root_info(dir)
        }

        fn read_batch(
            &self,
            dir: &Self::Handle,
            restart: bool,
            scratch: &mut Self::Scratch,
            emit: &mut dyn FnMut(DirEntryRef<'_>),
        ) -> io::Result<bool> {
            Win32DirSource.read_batch(dir, restart, scratch, emit)
        }
    }

    /// Temp tree `top.txt, empty\, sub\needle_alpha.txt, sub\inner\deep.txt, gate\late.txt,
    /// gone\x.txt`: returns its directory, display root, and a source gating `gate`.
    fn gated_tree(tag: &str) -> (PathBuf, String, Arc<GatedSource>) {
        let dir = std::env::temp_dir().join(format!(
            "fm_idx_{tag}_{}_{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        for d in ["empty", "sub\\inner", "gate", "gone"] {
            fs::create_dir_all(dir.join(d)).unwrap();
        }
        for f in [
            "top.txt",
            "sub\\needle_alpha.txt",
            "sub\\inner\\deep.txt",
            "gate\\late.txt",
            "gone\\x.txt",
        ] {
            fs::write(dir.join(f), b"x").unwrap();
        }
        let root = normalize_display_path(&dir.to_string_lossy());
        // NTFS refreshes a folder's mtime in its parent's listing lazily (on open): settle it
        // so walks run at different times list the same values.
        let settle = walk_with_source_report(
            &Win32DirSource,
            &root,
            WalkOptions::default(),
            &AtomicBool::new(false),
            |_| {},
        );
        assert!(settle.is_some());
        let src = Arc::new(GatedSource {
            gate: join_child_path(&root, "gate"),
            gone: join_child_path(&root, "gone"),
            open: (Mutex::new(false), Condvar::new()),
        });
        (dir, root, src)
    }

    fn finds(idx: &Index, q: &str) -> Vec<String> {
        let opts = SearchOptions {
            include_hidden: true,
            limit: 10,
        };
        let hits = idx.search_scoped(&parse_query(q), None, &opts, &CancelToken::new());
        hits.unwrap().into_iter().map(|h| idx.path(h.id)).collect()
    }

    fn listing(idx: &Index) -> Vec<(String, u32, u64, i64, u64)> {
        let mut v: Vec<_> = idx
            .iter_ids()
            .map(|id| {
                let m = idx.meta(id);
                (idx.path(id), m.attrs, m.size, m.mtime, m.file_id)
            })
            .collect();
        v.sort();
        v
    }

    /// One volume's walk thread, started over `root` from `live`.
    struct Walking {
        index: Arc<RwLock<Index>>,
        partial: Arc<AtomicBool>,
        dirty: Arc<AtomicBool>,
        cancel: Arc<AtomicBool>,
        coord: Arc<(Mutex<RewalkCoord>, Condvar)>,
        events: mpsc::Receiver<Event>,
        worker: JoinHandle<()>,
    }

    impl Walking {
        fn start(src: &Arc<GatedSource>, root: &str, live: Index) -> Self {
            let index = Arc::new(RwLock::new(live));
            let partial = Arc::new(AtomicBool::new(false));
            let dirty = Arc::new(AtomicBool::new(false));
            let cancel = Arc::new(AtomicBool::new(false));
            let coord = Arc::new(coord(false));
            coord.0.lock().unwrap().manual_or_timer_pending = true;
            let (tx, events) = mpsc::channel();
            let worker = {
                let (src, root) = (Arc::clone(src), root.to_owned());
                let (index, partial, dirty) =
                    (Arc::clone(&index), Arc::clone(&partial), Arc::clone(&dirty));
                let (cancel, coord) = (Arc::clone(&cancel), Arc::clone(&coord));
                thread::spawn(move || {
                    rewalk_worker(
                        &*src,
                        &root,
                        index,
                        Arc::new(Mutex::new(())),
                        Arc::new(RwLock::new(DeniedFilter::default())),
                        true,
                        dirty,
                        Arc::new(AtomicBool::new(false)),
                        partial,
                        cancel,
                        coord,
                        Duration::ZERO,
                        Arc::new(move |ev| {
                            let _ = tx.send(ev);
                        }),
                    );
                })
            };
            Self {
                index,
                partial,
                dirty,
                cancel,
                coord,
                events,
                worker,
            }
        }

        /// Waits up to 10 s for a `Changed` and then for `done`, failing if `Ready` comes first.
        fn changed_until(&self, done: impl Fn(&Index) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut changed = false;
            while !(changed && done(&self.index.read().unwrap())) {
                assert!(Instant::now() < deadline, "no progressive result in 10 s");
                match self.events.recv_timeout(Duration::from_millis(50)) {
                    Ok(Event::Ready { .. }) => panic!("Ready came before progressive results"),
                    Ok(Event::Changed) => changed = true,
                    _ => {}
                }
            }
        }

        fn wait_ready(&self) {
            loop {
                match self.events.recv_timeout(Duration::from_secs(10)) {
                    Ok(Event::Ready { .. }) => return,
                    Ok(_) => {}
                    Err(e) => panic!("no Ready: {e}"),
                }
            }
        }

        fn stop(self) -> Index {
            self.coord.0.lock().unwrap().stop = true;
            self.coord.1.notify_all();
            self.worker.join().unwrap();
            std::mem::take(&mut *self.index.write().unwrap())
        }
    }

    #[test]
    fn first_walk_is_searchable_before_ready_and_ends_like_a_plain_walk() {
        let (dir, root, src) = gated_tree("progressive");
        let w = Walking::start(&src, &root, Index::new());
        let needle = join_child_path(&root, "sub\\needle_alpha.txt");
        w.changed_until(|idx| finds(idx, "needle_alpha") == [needle.clone()]);
        {
            let idx = w.index.read().unwrap();
            let at = |rel: &str| idx.lookup(&join_child_path(&root, rel));
            assert!(at("empty").is_some(), "an empty folder shows with its own listing");
            assert!(at("sub\\inner\\deep.txt").is_some());
            assert_eq!(at("gate\\late.txt"), None, "the walk is still gated");
            assert_eq!(at("gone"), None, "a folder that failed to list never shows");
            assert_eq!(idx.volume_serial(), 0, "a partial index must not load as a snapshot");
        }
        assert!(w.partial.load(Ordering::Acquire), "folder sizes wait for the merge");
        src.release();
        w.wait_ready();
        assert!(!w.partial.load(Ordering::Acquire) && w.dirty.load(Ordering::Acquire));
        let live = w.stop();

        // What the non-progressive first walk produced: the fresh index merged into an empty one.
        let (fresh, _, report) = walk_with_source_report(
            &*src,
            &root,
            WalkOptions::default(),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        let mut plain = Index::new();
        merge_index_with_report(&mut plain, fresh, &report, true);
        assert_eq!(live.len(), plain.len());
        assert_eq!(listing(&live), listing(&plain));
        assert_eq!(live.volume_serial(), plain.volume_serial());
        assert!(live.lookup(&join_child_path(&root, "gate\\late.txt")).is_some());
        assert_eq!(live.lookup(&join_child_path(&root, "gone")), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewalk_over_an_existing_index_is_not_progressive() {
        let (dir, root, src) = gated_tree("rewalk");
        let mut live = Index::new();
        let r = live.add_root(&root);
        live.insert(r, "stale.txt", meta(ARC, 0));
        let w = Walking::start(&src, &root, live);
        // Gated for two insert intervals: a progressive walk would insert at its first tick.
        let until = Instant::now() + PROGRESSIVE_INTERVAL * 2;
        let mut ticks = 0;
        while Instant::now() < until {
            match w.events.recv_timeout(Duration::from_millis(50)) {
                Ok(Event::Progress { .. }) => ticks += 1,
                Ok(ev) => panic!("{ev:?} while the re-walk is gated"),
                Err(_) => {}
            }
        }
        assert!(ticks >= 3, "{ticks} progress ticks");
        let needle = join_child_path(&root, "sub\\needle_alpha.txt");
        {
            let idx = w.index.read().unwrap();
            assert_eq!(idx.len(), 2);
            assert_eq!(idx.lookup(&needle), None);
        }
        assert!(!w.partial.load(Ordering::Acquire));
        src.release();
        w.wait_ready();
        let idx = w.stop();
        assert!(idx.lookup(&needle).is_some());
        assert_eq!(idx.lookup(&join_child_path(&root, "stale.txt")), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn walk_batches_show_denied_folders_filtered_and_never_failed_ones() {
        let batch = |rel: &str, fid, failed, denied, children: Vec<(String, Meta, bool)>| {
            WalkBatch {
                rel_path: rel.to_owned(),
                meta: meta(DIRECTORY, fid),
                failed,
                denied,
                children,
            }
        };
        let kid = |name: &str, attrs, fid, descend| (name.to_owned(), meta(attrs, fid), descend);
        let batches = vec![
            batch(
                "",
                1,
                false,
                false,
                vec![
                    kid("Secret", DIRECTORY, 2, true),
                    kid("Gone", DIRECTORY, 3, true),
                    kid("a.txt", ARC, 4, false),
                ],
            ),
            batch("Secret", 2, false, true, Vec::new()),
            batch("Gone", 3, true, false, Vec::new()),
        ];
        let index = RwLock::new(Index::new());
        let denied = RwLock::new(DeniedFilter::default());
        assert!(insert_walk_batches("R:\\", &index, &Mutex::new(()), &denied, batches));
        let idx = index.into_inner().unwrap();
        assert!(idx.lookup("R:\\Secret").is_some_and(|s| idx.is_dir(s)));
        assert!(idx.lookup("R:\\a.txt").is_some());
        assert_eq!(idx.lookup("R:\\Gone"), None, "a failed folder that listed nothing");
        assert_eq!(idx.len(), 3);
        let under = rec(NotifyAction::Added, "Secret\\leak.txt", 9, 2, ARC);
        let df = denied.into_inner().unwrap();
        for use_file_id in [true, false] {
            assert!(df.is_record_denied(Some((&idx, 0)), &under, use_file_id));
        }
        let beside = rec(NotifyAction::Added, "b.txt", 10, 1, ARC);
        assert!(!df.is_record_denied(Some((&idx, 0)), &beside, true));
    }

    #[test]
    fn cancelled_first_walk_stops_cleanly_and_its_snapshot_does_not_load() {
        let (dir, root, src) = gated_tree("cancel");
        let w = Walking::start(&src, &root, Index::new());
        w.changed_until(|idx| !finds(idx, "needle_alpha").is_empty());
        w.cancel.store(true, Ordering::Release);
        // The gated worker must return for the walk to end (a real listing would).
        src.release();
        let partial = Arc::clone(&w.partial);
        let live = w.stop();
        assert!(partial.load(Ordering::Acquire));
        assert!(!live.is_empty());
        assert_eq!(live.volume_serial(), 0);

        // Saved at exit, the partial index carries serial 0 and is rejected on the next start.
        let serial = resolve_volume_serial(&root);
        assert_ne!(serial, 0, "needs a volume with a serial");
        let data_dir = dir.join("data");
        fs::create_dir_all(&data_dir).unwrap();
        let saved = RwLock::new(live);
        assert!(matches!(
            save_volume_snapshot(&data_dir, serial, &saved, &AtomicBool::new(true)),
            Ok(true)
        ));
        let path = snapshot_path(&data_dir, serial);
        assert!(load_snapshot_for_root(&path, serial, &root).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
