//! Multi-volume background indexing, watching, snapshot persistence, and ranked search service.

use std::collections::HashMap;
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
    WalkFoldersReport, WalkOptions, merge_index_with_report, walk_root_report,
};
use crate::win::watcher::{
    DeniedFilter, NoopWatcherFs, NotifyRecord, VolumeWatcher, WatcherConfig, Win32WatcherFs,
    apply_records_filtered, batch_needs_prefetch, collect_parent_hints,
    prefetch_batch_outside_lock,
};
use crate::win::{
    is_ntfs_volume, lock_or_recover, normalize_display_path, open_dir_handle,
    query_dir_info_by_handle, read_or_recover, to_extended_path, wait_or_recover,
    wait_timeout_or_recover, wide_null, win32_to_io, write_or_recover,
};

const DRIVE_FIXED: u32 = 3;
const HOURLY_SAVE_INTERVAL: Duration = Duration::from_secs(3600);
const DAILY_REWALK_INTERVAL: Duration = Duration::from_secs(86400);
const HOUSEKEEP_TICK: Duration = Duration::from_secs(60);
const MAX_DURING_WALK_BUFFER: usize = 100_000;

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
}

struct VolumeState {
    volume_serial: u32,
    index: Arc<RwLock<Index>>,
    dirty: Arc<AtomicBool>,
    scanning: Arc<AtomicBool>,
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
                    Box::new(move |records| {
                        let (mu, _) = &*coord_locked;
                        let mut st = lock_or_recover(mu);
                        if st.collecting {
                            if st.during_walk.len().saturating_add(records.len())
                                <= MAX_DURING_WALK_BUFFER
                            {
                                st.during_walk.extend_from_slice(records);
                            } else {
                                st.overflowed_during_walk = true;
                                st.overflow_pending = true;
                            }
                        }
                    }),
                    Box::new(move |_records, changed| {
                        if changed {
                            dirty_applied.store(true, Ordering::Release);
                            ev_applied(Event::Changed);
                        }
                    }),
                    Box::new(move || {
                        let (mu, cv) = &*coord_overflow;
                        let mut st = lock_or_recover(mu);
                        if !st.stop {
                            st.overflow_pending = true;
                            if st.collecting {
                                st.overflowed_during_walk = true;
                            }
                            cv.notify_one();
                        }
                    }),
                    Box::new(move || {
                        let (mu, cv) = &*coord_restarted;
                        let mut st = lock_or_recover(mu);
                        if !st.stop {
                            st.manual_or_timer_pending = true;
                            cv.notify_one();
                        }
                    }),
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
                let cancel_w = Arc::clone(&rewalk_cancel);
                let coord_w = Arc::clone(&rewalk_coord);
                let ev_w = Arc::clone(&on_event);
                let overflow_cd = watcher_config.overflow_rewalk_cooldown;

                thread::Builder::new()
                    .name(format!("fm-rewalk:{root_w}"))
                    .spawn(move || {
                        rewalk_worker(
                            &root_w,
                            index_w,
                            apply_w,
                            denied_w,
                            use_file_id,
                            dirty_w,
                            scanning_w,
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
            if matches!(
                save_volume_snapshot(&self.data_dir, vol.volume_serial, &vol.index),
                Ok(true)
            ) {
                vol.dirty.store(false, Ordering::Release);
            }
        }
    }

    /// Triggers an immediate background re-walk for every managed volume (bypassing overflow cooldown).
    pub fn trigger_rewalk(&self) {
        for vol in &self.volumes {
            let (mu, cv) = &*vol.rewalk_coord;
            let mut st = lock_or_recover(mu);
            if !st.stop {
                st.manual_or_timer_pending = true;
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
            if matches!(
                save_volume_snapshot(&self.data_dir, vol.volume_serial, &vol.index),
                Ok(true)
            ) {
                vol.dirty.store(false, Ordering::Release);
            }
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
                    if t.dirty.load(Ordering::Acquire)
                        && matches!(
                            save_volume_snapshot(&data_dir, t.volume_serial, &t.index),
                            Ok(true)
                        )
                    {
                        t.dirty.store(false, Ordering::Release);
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
fn rewalk_worker(
    root: &str,
    index: Arc<RwLock<Index>>,
    apply_mu: Arc<Mutex<()>>,
    denied_filter: Arc<RwLock<DeniedFilter>>,
    use_file_id: bool,
    dirty: Arc<AtomicBool>,
    scanning: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    coord: Arc<(Mutex<RewalkCoord>, Condvar)>,
    overflow_cooldown: Duration,
    on_event: Arc<dyn Fn(Event) + Send + Sync>,
) {
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
                    if let Some(last) = st.last_finished
                        && !overflow_cooldown.is_zero()
                    {
                        let ready_at = last + overflow_cooldown;
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
        let walk_res = walk_root_report(root, walk_opts, &cancel, |entries| {
            on_event(Event::Progress {
                root: root.to_owned(),
                entries,
            });
        });

        let Some((fresh, stats, walk_report)) = walk_res else {
            let mut st = lock_or_recover(mu);
            st.running = false;
            st.collecting = false;
            st.during_walk.clear();
            scanning.store(false, Ordering::Release);
            if st.stop || cancel.load(Ordering::Acquire) {
                break;
            }
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
            &fs,
        );

        if changed {
            dirty.store(true, Ordering::Release);
        }

        let next_manual_immediate = {
            let mut st = lock_or_recover(mu);
            st.running = false;
            st.last_finished = Some(Instant::now());
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
    fs: &dyn crate::win::watcher::WatcherFs,
) -> (bool, usize) {
    let _apply_guard = lock_or_recover(apply_mu);

    let (replay, mut changed, needs_prefetch, parent_hints, mut denied_snap, live_len) = {
        let mut live = write_or_recover(index);
        let replay = {
            let mut st = lock_or_recover(coord_mu);
            st.collecting = false;
            if st.overflowed_during_walk {
                st.overflow_pending = true;
            }
            std::mem::take(&mut st.during_walk)
        };

        let (mut ch, active_denied) = if walk_report.root_failed {
            (false, read_or_recover(denied_filter).clone())
        } else {
            let new_denied = DeniedFilter::from_walk(&walk_report.denied);
            *write_or_recover(denied_filter) = new_denied.clone();
            let ch = merge_index_with_report(&mut live, fresh, walk_report, use_file_id);
            (ch, new_denied)
        };

        if replay.is_empty() {
            (
                Vec::new(),
                ch,
                false,
                HashMap::new(),
                active_denied,
                live.len(),
            )
        } else if !batch_needs_prefetch(&replay) {
            if let Some(root_id) = live.lookup(root) {
                ch |= apply_records_filtered(
                    &mut live,
                    root_id,
                    &replay,
                    &NoopWatcherFs,
                    &active_denied,
                    use_file_id,
                );
            }
            (
                Vec::new(),
                ch,
                false,
                HashMap::new(),
                active_denied,
                live.len(),
            )
        } else {
            let hints = if use_file_id {
                live.lookup(root)
                    .map(|rid| collect_parent_hints(&live, rid, &replay))
                    .unwrap_or_default()
            } else {
                HashMap::new()
            };
            (replay, ch, true, hints, active_denied, live.len())
        }
    };

    if !needs_prefetch || replay.is_empty() {
        return (changed, live_len);
    }

    // List Added directory subtrees and resolve 8.3 short names strictly outside index.write().
    let prefetched =
        prefetch_batch_outside_lock(root, &replay, &parent_hints, &denied_snap, use_file_id, fs);
    if !prefetched.extra_denied.is_empty() {
        denied_snap.extend_from(&prefetched.extra_denied);
        write_or_recover(denied_filter).extend_from(&prefetched.extra_denied);
    }

    let mut live = write_or_recover(index);
    if let Some(root_id) = live.lookup(root) {
        changed |= apply_records_filtered(
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

fn save_volume_snapshot(
    data_dir: &Path,
    volume_serial: u32,
    index: &RwLock<Index>,
) -> io::Result<bool> {
    let dst_path = snapshot_path(data_dir, volume_serial);
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_path = data_dir.join(format!(
        "index-{volume_serial:08x}.bin.tmp.{}.{seq}",
        std::process::id()
    ));
    let tmp_ext = to_extended_path(&tmp_path.to_string_lossy());
    let dst_ext = to_extended_path(&dst_path.to_string_lossy());

    let write_res: io::Result<bool> = (|| {
        let guard = read_or_recover(index);
        if guard.is_empty() {
            return Ok(false);
        }
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
        let _ = fs::remove_file(&tmp_ext);
    }
    write_res
}

#[cfg(test)]
mod tests {
    use fm_fs::attrs::DIRECTORY;

    use super::*;
    use crate::Meta;
    use crate::win::walker::WalkFolderRef;
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
            &NoopWatcherFs,
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
}
