// Copyright 2016 TiKV Project Authors. Licensed under Apache-2.0.

use std::collections::Bound::{Excluded, Included, Unbounded};
use std::collections::{BTreeMap, VecDeque};
use std::fmt::{self, Display, Formatter};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::u64;

use engine::Engines;
use engine_rocks::{Compat, RocksEngine, RocksSnapshot};
use engine_traits::CF_RAFT;
use engine_traits::{MiscExt, Mutable, Peekable, WriteBatchExt};
use kvproto::raft_serverpb::{PeerState, RaftApplyState, RegionLocalState};
use raft::eraftpb::Snapshot as RaftSnapshot;

use crate::coprocessor::CoprocessorHost;
use crate::store::peer_storage::{
    JOB_STATUS_CANCELLED, JOB_STATUS_CANCELLING, JOB_STATUS_FAILED, JOB_STATUS_FINISHED,
    JOB_STATUS_PENDING, JOB_STATUS_RUNNING,
};
use crate::store::snap::{plain_file_used, Error, Result, SNAPSHOT_CFS};
use crate::store::transport::CasualRouter;
use crate::store::{
    self, check_abort, ApplyOptions, CasualMessage, SnapEntry, SnapKey, SnapManager,
};
use yatp::pool::{Builder, ThreadPool};
use yatp::task::future::TaskCell;

use tikv_util::time;
use tikv_util::timer::Timer;
use tikv_util::worker::{Runnable, RunnableWithTimer};

use super::metrics::*;

const GENERATE_POOL_SIZE: usize = 2;

// used to periodically check whether we should delete a stale peer's range in region runner
pub const STALE_PEER_CHECK_INTERVAL: u64 = 10_000; // 10000 milliseconds

// used to periodically check whether schedule pending applies in region runner
pub const PENDING_APPLY_CHECK_INTERVAL: u64 = 1_000; // 1000 milliseconds

const CLEANUP_MAX_DURATION: Duration = Duration::from_secs(5);

/// Region related task
#[derive(Debug)]
pub enum Task {
    Gen {
        region_id: u64,
        last_applied_index_term: u64,
        last_applied_state: RaftApplyState,
        kv_snap: RocksSnapshot,
        notifier: SyncSender<RaftSnapshot>,
    },
    Apply {
        region_id: u64,
        status: Arc<AtomicUsize>,
    },
    /// Destroy data between [start_key, end_key).
    ///
    /// The deletion may and may not succeed.
    Destroy {
        region_id: u64,
        start_key: Vec<u8>,
        end_key: Vec<u8>,
    },
}

impl Task {
    pub fn destroy(region_id: u64, start_key: Vec<u8>, end_key: Vec<u8>) -> Task {
        Task::Destroy {
            region_id,
            start_key,
            end_key,
        }
    }
}

impl Display for Task {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match *self {
            Task::Gen { region_id, .. } => write!(f, "Snap gen for {}", region_id),
            Task::Apply { region_id, .. } => write!(f, "Snap apply for {}", region_id),
            Task::Destroy {
                region_id,
                ref start_key,
                ref end_key,
            } => write!(
                f,
                "Destroy {} [{}, {})",
                region_id,
                log_wrappers::Value::key(&start_key),
                log_wrappers::Value::key(&end_key)
            ),
        }
    }
}

#[derive(Clone)]
struct StalePeerInfo {
    // the start_key is stored as a key in PendingDeleteRanges
    // below are stored as a value in PendingDeleteRanges
    pub region_id: u64,
    pub end_key: Vec<u8>,
    pub timeout: time::Instant,
}

/// A structure records all ranges to be deleted with some delay.
/// The delay is because there may be some coprocessor requests related to these ranges.
#[derive(Clone, Default)]
struct PendingDeleteRanges {
    ranges: BTreeMap<Vec<u8>, StalePeerInfo>, // start_key -> StalePeerInfo
}

impl PendingDeleteRanges {
    /// Finds ranges that overlap with [start_key, end_key).
    fn find_overlap_ranges(
        &self,
        start_key: &[u8],
        end_key: &[u8],
    ) -> Vec<(u64, Vec<u8>, Vec<u8>)> {
        let mut ranges = Vec::new();
        // find the first range that may overlap with [start_key, end_key)
        let sub_range = self.ranges.range((Unbounded, Excluded(start_key.to_vec())));
        if let Some((s_key, peer_info)) = sub_range.last() {
            if peer_info.end_key > start_key.to_vec() {
                ranges.push((
                    peer_info.region_id,
                    s_key.clone(),
                    peer_info.end_key.clone(),
                ));
            }
        }

        // find the rest ranges that overlap with [start_key, end_key)
        for (s_key, peer_info) in self
            .ranges
            .range((Included(start_key.to_vec()), Excluded(end_key.to_vec())))
        {
            ranges.push((
                peer_info.region_id,
                s_key.clone(),
                peer_info.end_key.clone(),
            ));
        }
        ranges
    }

    /// Gets ranges that overlap with [start_key, end_key).
    pub fn drain_overlap_ranges(
        &mut self,
        start_key: &[u8],
        end_key: &[u8],
    ) -> Vec<(u64, Vec<u8>, Vec<u8>)> {
        let ranges = self.find_overlap_ranges(start_key, end_key);

        for &(_, ref s_key, _) in &ranges {
            self.ranges.remove(s_key).unwrap();
        }
        ranges
    }

    /// Removes and returns the peer info with the `start_key`.
    fn remove(&mut self, start_key: &[u8]) -> Option<(u64, Vec<u8>, Vec<u8>)> {
        self.ranges
            .remove(start_key)
            .map(|peer_info| (peer_info.region_id, start_key.to_owned(), peer_info.end_key))
    }

    /// Inserts a new range waiting to be deleted.
    ///
    /// Before an insert is called, it must call drain_overlap_ranges to clean the overlapping range.
    fn insert(&mut self, region_id: u64, start_key: &[u8], end_key: &[u8], timeout: time::Instant) {
        if !self.find_overlap_ranges(&start_key, &end_key).is_empty() {
            panic!(
                "[region {}] register deleting data in [{}, {}) failed due to overlap",
                region_id,
                log_wrappers::Value::key(&start_key),
                log_wrappers::Value::key(&end_key),
            );
        }
        let info = StalePeerInfo {
            region_id,
            end_key: end_key.to_owned(),
            timeout,
        };
        self.ranges.insert(start_key.to_owned(), info);
    }

    /// Gets all timeout ranges info.
    pub fn timeout_ranges<'a>(
        &'a self,
        now: time::Instant,
    ) -> impl Iterator<Item = (u64, Vec<u8>, Vec<u8>)> + 'a {
        self.ranges
            .iter()
            .filter(move |&(_, info)| info.timeout <= now)
            .map(|(start_key, info)| (info.region_id, start_key.clone(), info.end_key.clone()))
    }

    pub fn len(&self) -> usize {
        self.ranges.len()
    }
}

#[derive(Clone)]
struct SnapContext<R> {
    engines: Engines,
    batch_size: usize,
    mgr: SnapManager,
    use_delete_range: bool,
    clean_stale_peer_delay: Duration,
    pending_delete_ranges: PendingDeleteRanges,
    coprocessor_host: CoprocessorHost,
    router: R,
}

impl<R: CasualRouter<RocksEngine>> SnapContext<R> {
    /// Generates the snapshot of the Region.
    fn generate_snap(
        &self,
        region_id: u64,
        last_applied_index_term: u64,
        last_applied_state: RaftApplyState,
        kv_snap: RocksSnapshot,
        notifier: SyncSender<RaftSnapshot>,
    ) -> Result<()> {
        // do we need to check leader here?
        let snap = box_try!(store::do_snapshot::<RocksEngine>(
            self.mgr.clone(),
            kv_snap,
            region_id,
            last_applied_index_term,
            last_applied_state,
        ));
        // Only enable the fail point when the region id is equal to 1, which is
        // the id of bootstrapped region in tests.
        fail_point!("region_gen_snap", region_id == 1, |_| Ok(()));
        if let Err(e) = notifier.try_send(snap) {
            info!(
                "failed to notify snap result, leadership may have changed, ignore error";
                "region_id" => region_id,
                "err" => %e,
            );
        }
        // The error can be ignored as snapshot will be sent in next heartbeat in the end.
        let _ = self
            .router
            .send(region_id, CasualMessage::SnapshotGenerated);
        Ok(())
    }

    /// Handles the task of generating snapshot of the Region. It calls `generate_snap` to do the actual work.
    fn handle_gen(
        &self,
        region_id: u64,
        last_applied_index_term: u64,
        last_applied_state: RaftApplyState,
        kv_snap: RocksSnapshot,
        notifier: SyncSender<RaftSnapshot>,
    ) {
        SNAP_COUNTER_VEC
            .with_label_values(&["generate", "all"])
            .inc();
        let gen_histogram = SNAP_HISTOGRAM.with_label_values(&["generate"]);
        let timer = gen_histogram.start_coarse_timer();

        if let Err(e) = self.generate_snap(
            region_id,
            last_applied_index_term,
            last_applied_state,
            kv_snap,
            notifier,
        ) {
            error!(%e; "failed to generate snap!!!"; "region_id" => region_id,);
            return;
        }

        SNAP_COUNTER_VEC
            .with_label_values(&["generate", "success"])
            .inc();
        timer.observe_duration();
    }

    /// Applies snapshot data of the Region.
    fn apply_snap(&mut self, region_id: u64, abort: Arc<AtomicUsize>) -> Result<()> {
        info!("begin apply snap data"; "region_id" => region_id);
        fail_point!("region_apply_snap", |_| { Ok(()) });
        check_abort(&abort)?;
        let region_key = keys::region_state_key(region_id);
        let mut region_state: RegionLocalState =
            match box_try!(self.engines.kv.c().get_msg_cf(CF_RAFT, &region_key)) {
                Some(state) => state,
                None => {
                    return Err(box_err!(
                        "failed to get region_state from {}",
                        log_wrappers::Value::key(&region_key)
                    ));
                }
            };

        // clear up origin data.
        let region = region_state.get_region().clone();
        let start_key = keys::enc_start_key(&region);
        let end_key = keys::enc_end_key(&region);
        check_abort(&abort)?;
        self.cleanup_overlap_ranges(&start_key, &end_key);
        box_try!(self.engines.kv.c().delete_all_in_range(
            &start_key,
            &end_key,
            self.use_delete_range
        ));
        check_abort(&abort)?;
        fail_point!("apply_snap_cleanup_range");

        let state_key = keys::apply_state_key(region_id);
        let apply_state: RaftApplyState =
            match box_try!(self.engines.kv.c().get_msg_cf(CF_RAFT, &state_key)) {
                Some(state) => state,
                None => {
                    return Err(box_err!(
                        "failed to get raftstate from {}",
                        log_wrappers::Value::key(&state_key)
                    ));
                }
            };
        let term = apply_state.get_truncated_state().get_term();
        let idx = apply_state.get_truncated_state().get_index();
        let snap_key = SnapKey::new(region_id, term, idx);
        self.mgr.register(snap_key.clone(), SnapEntry::Applying);
        defer!({
            self.mgr.deregister(&snap_key, &SnapEntry::Applying);
        });
        let mut s = box_try!(self.mgr.get_snapshot_for_applying_to_engine(&snap_key));
        if !s.exists() {
            return Err(box_err!("missing snapshot file {}", s.path()));
        }
        check_abort(&abort)?;
        let timer = Instant::now();
        let options = ApplyOptions {
            db: self.engines.kv.c().clone(),
            region,
            abort: Arc::clone(&abort),
            write_batch_size: self.batch_size,
            coprocessor_host: self.coprocessor_host.clone(),
        };
        s.apply(options)?;

        let mut wb = self.engines.kv.c().write_batch();
        region_state.set_state(PeerState::Normal);
        box_try!(wb.put_msg_cf(CF_RAFT, &region_key, &region_state));
        box_try!(wb.delete_cf(CF_RAFT, &keys::snapshot_raft_state_key(region_id)));
        self.engines.kv.c().write(&wb).unwrap_or_else(|e| {
            panic!("{} failed to save apply_snap result: {:?}", region_id, e);
        });
        info!(
            "apply new data";
            "region_id" => region_id,
            "time_takes" => ?timer.elapsed(),
        );
        Ok(())
    }

    /// Tries to apply the snapshot of the specified Region. It calls `apply_snap` to do the actual work.
    fn handle_apply(&mut self, region_id: u64, status: Arc<AtomicUsize>) {
        status.compare_and_swap(JOB_STATUS_PENDING, JOB_STATUS_RUNNING, Ordering::SeqCst);
        SNAP_COUNTER_VEC.with_label_values(&["apply", "all"]).inc();
        let apply_histogram = SNAP_HISTOGRAM.with_label_values(&["apply"]);
        let timer = apply_histogram.start_coarse_timer();

        match self.apply_snap(region_id, Arc::clone(&status)) {
            Ok(()) => {
                status.swap(JOB_STATUS_FINISHED, Ordering::SeqCst);
                SNAP_COUNTER_VEC
                    .with_label_values(&["apply", "success"])
                    .inc();
            }
            Err(Error::Abort) => {
                warn!("applying snapshot is aborted"; "region_id" => region_id);
                assert_eq!(
                    status.swap(JOB_STATUS_CANCELLED, Ordering::SeqCst),
                    JOB_STATUS_CANCELLING
                );
                SNAP_COUNTER_VEC
                    .with_label_values(&["apply", "abort"])
                    .inc();
            }
            Err(e) => {
                error!(%e; "failed to apply snap!!!");

                status.swap(JOB_STATUS_FAILED, Ordering::SeqCst);
                SNAP_COUNTER_VEC.with_label_values(&["apply", "fail"]).inc();
            }
        }

        timer.observe_duration();
    }

    /// Cleans up the data within the range.
    fn cleanup_range(
        &self,
        region_id: u64,
        start_key: &[u8],
        end_key: &[u8],
        use_delete_files: bool,
    ) {
        if use_delete_files {
            if let Err(e) = self
                .engines
                .kv
                .c()
                .delete_all_files_in_range(start_key, end_key)
            {
                error!(%e;
                    "failed to delete files in range";
                    "region_id" => region_id,
                    "start_key" => log_wrappers::Value::key(start_key),
                    "end_key" => log_wrappers::Value::key(end_key),
                );
                return;
            }
        }
        if let Err(e) =
            self.engines
                .kv
                .c()
                .delete_all_in_range(start_key, end_key, self.use_delete_range)
        {
            error!(%e;
                "failed to delete data in range";
                "region_id" => region_id,
                "start_key" => log_wrappers::Value::key(start_key),
                "end_key" => log_wrappers::Value::key(end_key),
            );
        }
        if use_delete_files {
            if let Err(e) = self
                .engines
                .kv
                .c()
                .delete_blob_files_in_range(start_key, end_key)
            {
                error!(%e;
                    "failed to delete blob files in range";
                    "region_id" => region_id,
                    "start_key" => log_wrappers::Value::key(start_key),
                    "end_key" => log_wrappers::Value::key(end_key),
                );
                return;
            }
        }
        info!(
            "succeed in deleting data in range";
            "region_id" => region_id,
            "start_key" => log_wrappers::Value::key(start_key),
            "end_key" => log_wrappers::Value::key(end_key),
        );
    }

    /// Gets the overlapping ranges and cleans them up.
    fn cleanup_overlap_ranges(&mut self, start_key: &[u8], end_key: &[u8]) {
        let overlap_ranges = self
            .pending_delete_ranges
            .drain_overlap_ranges(start_key, end_key);
        let use_delete_files = false;
        for (region_id, s_key, e_key) in overlap_ranges {
            self.cleanup_range(region_id, &s_key, &e_key, use_delete_files);
        }
    }

    /// Inserts a new pending range, and it will be cleaned up with some delay.
    fn insert_pending_delete_range(
        &mut self,
        region_id: u64,
        start_key: &[u8],
        end_key: &[u8],
    ) -> bool {
        if self.clean_stale_peer_delay.as_secs() == 0 {
            return false;
        }

        self.cleanup_overlap_ranges(start_key, end_key);

        info!(
            "register deleting data in range";
            "region_id" => region_id,
            "start_key" => log_wrappers::Value::key(start_key),
            "end_key" => log_wrappers::Value::key(end_key),
        );
        let timeout = time::Instant::now() + self.clean_stale_peer_delay;
        self.pending_delete_ranges
            .insert(region_id, start_key, end_key, timeout);
        true
    }

    /// Cleans up timeouted ranges.
    fn clean_timeout_ranges(&mut self) {
        STALE_PEER_PENDING_DELETE_RANGE_GAUGE.set(self.pending_delete_ranges.len() as f64);

        let now = time::Instant::now();
        let mut cleaned_range_keys = vec![];
        {
            let use_delete_files = true;
            for (region_id, start_key, end_key) in self.pending_delete_ranges.timeout_ranges(now) {
                self.cleanup_range(
                    region_id,
                    start_key.as_slice(),
                    end_key.as_slice(),
                    use_delete_files,
                );
                cleaned_range_keys.push(start_key);
                let elapsed = now.elapsed();
                if elapsed >= CLEANUP_MAX_DURATION {
                    let len = cleaned_range_keys.len();
                    let elapsed = elapsed.as_millis() as f64 / 1000f64;
                    info!("clean timeout ranges, now backoff"; "key_count" => len, "time_takes" => elapsed);
                    break;
                }
            }
        }
        for key in cleaned_range_keys {
            assert!(
                self.pending_delete_ranges.remove(&key).is_some(),
                "cleanup pending_delete_ranges {} should exist",
                log_wrappers::Value::key(&key)
            );
        }
    }

    /// Checks the number of files at level 0 to avoid write stall after ingesting sst.
    /// Returns true if the ingestion causes write stall.
    fn ingest_maybe_stall(&self) -> bool {
        for cf in SNAPSHOT_CFS {
            // no need to check lock cf
            if plain_file_used(cf) {
                continue;
            }
            if engine_rocks::util::ingest_maybe_slowdown_writes(&self.engines.kv, cf) {
                return true;
            }
        }
        false
    }
}

pub struct Runner<R> {
    pool: ThreadPool<TaskCell>,
    ctx: SnapContext<R>,
    // we may delay some apply tasks if level 0 files to write stall threshold,
    // pending_applies records all delayed apply task, and will check again later
    pending_applies: VecDeque<Task>,
}

impl<R: CasualRouter<RocksEngine>> Runner<R> {
    pub fn new(
        engines: Engines,
        mgr: SnapManager,
        batch_size: usize,
        use_delete_range: bool,
        clean_stale_peer_delay: Duration,
        coprocessor_host: CoprocessorHost,
        router: R,
    ) -> Runner<R> {
        Runner {
            pool: Builder::new(thd_name!("snap-generator"))
                .max_thread_count(GENERATE_POOL_SIZE)
                .build_future_pool(),

            ctx: SnapContext {
                engines,
                mgr,
                batch_size,
                use_delete_range,
                clean_stale_peer_delay,
                pending_delete_ranges: PendingDeleteRanges::default(),
                coprocessor_host,
                router,
            },
            pending_applies: VecDeque::new(),
        }
    }

    pub fn new_timer(&self) -> Timer<Event> {
        let mut timer = Timer::new(2);
        timer.add_task(
            Duration::from_millis(PENDING_APPLY_CHECK_INTERVAL),
            Event::CheckApply,
        );
        timer.add_task(
            Duration::from_millis(STALE_PEER_CHECK_INTERVAL),
            Event::CheckStalePeer,
        );
        timer
    }

    /// Tries to apply pending tasks if there is some.
    fn handle_pending_applies(&mut self) {
        fail_point!("apply_pending_snapshot", |_| {});
        while !self.pending_applies.is_empty() {
            // should not handle too many applies than the number of files that can be ingested.
            // check level 0 every time because we can not make sure how does the number of level 0 files change.
            if self.ctx.ingest_maybe_stall() {
                break;
            }
            if let Some(Task::Apply { region_id, status }) = self.pending_applies.pop_front() {
                self.ctx.handle_apply(region_id, status);
            }
        }
    }
}

impl<R> Runnable<Task> for Runner<R>
where
    R: CasualRouter<RocksEngine> + Send + Clone + 'static,
{
    fn run(&mut self, task: Task) {
        match task {
            Task::Gen {
                region_id,
                last_applied_index_term,
                last_applied_state,
                kv_snap,
                notifier,
            } => {
                // It is safe for now to handle generating and applying snapshot concurrently,
                // but it may not when merge is implemented.
                let ctx = self.ctx.clone();

                self.pool.spawn(async move {
                    ctx.handle_gen(
                        region_id,
                        last_applied_index_term,
                        last_applied_state,
                        kv_snap,
                        notifier,
                    );
                });
            }
            task @ Task::Apply { .. } => {
                // to makes sure appling snapshots in order.
                self.pending_applies.push_back(task);
                self.handle_pending_applies();
                if !self.pending_applies.is_empty() {
                    // delay the apply and retry later
                    SNAP_COUNTER_VEC
                        .with_label_values(&["apply", "delay"])
                        .inc();
                }
            }
            Task::Destroy {
                region_id,
                start_key,
                end_key,
            } => {
                // try to delay the range deletion because
                // there might be a coprocessor request related to this range
                if !self
                    .ctx
                    .insert_pending_delete_range(region_id, &start_key, &end_key)
                {
                    self.ctx.cleanup_range(
                        region_id, &start_key, &end_key, false, /* use_delete_files */
                    );
                }
            }
        }
    }

    fn shutdown(&mut self) {
        self.pool.shutdown();
    }
}

/// Region related timeout event
pub enum Event {
    CheckStalePeer,
    CheckApply,
}

impl<R> RunnableWithTimer<Task, Event> for Runner<R>
where
    R: CasualRouter<RocksEngine> + Send + Clone + 'static,
{
    fn on_timeout(&mut self, timer: &mut Timer<Event>, event: Event) {
        match event {
            Event::CheckApply => {
                self.handle_pending_applies();
                timer.add_task(
                    Duration::from_millis(PENDING_APPLY_CHECK_INTERVAL),
                    Event::CheckApply,
                );
            }
            Event::CheckStalePeer => {
                self.ctx.clean_timeout_ranges();
                timer.add_task(
                    Duration::from_millis(STALE_PEER_CHECK_INTERVAL),
                    Event::CheckStalePeer,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::Duration;

    use crate::coprocessor::CoprocessorHost;
    use crate::store::peer_storage::JOB_STATUS_PENDING;
    use crate::store::snap::tests::get_test_db_for_regions;
    use crate::store::worker::RegionRunner;
    use crate::store::{CasualMessage, SnapKey, SnapManager};
    use engine::rocks;
    use engine::rocks::{ColumnFamilyOptions, Writable};
    use engine::Engines;
    use engine_rocks::{Compat, RocksSnapshot};
    use engine_traits::{Mutable, Peekable, WriteBatchExt};
    use engine_traits::{CF_DEFAULT, CF_RAFT};
    use kvproto::raft_serverpb::{PeerState, RaftApplyState, RegionLocalState};
    use raft::eraftpb::Entry;
    use tempfile::Builder;
    use tikv_util::time;
    use tikv_util::timer::Timer;
    use tikv_util::worker::Worker;

    use super::Event;
    use super::PendingDeleteRanges;
    use super::Task;

    fn insert_range(
        pending_delete_ranges: &mut PendingDeleteRanges,
        id: u64,
        s: &str,
        e: &str,
        timeout: time::Instant,
    ) {
        pending_delete_ranges.insert(id, s.as_bytes(), e.as_bytes(), timeout);
    }

    #[test]
    fn test_pending_delete_ranges() {
        let mut pending_delete_ranges = PendingDeleteRanges::default();
        let delay = Duration::from_millis(100);
        let id = 0;

        let timeout = time::Instant::now() + delay;
        insert_range(&mut pending_delete_ranges, id, "a", "c", timeout);
        insert_range(&mut pending_delete_ranges, id, "m", "n", timeout);
        insert_range(&mut pending_delete_ranges, id, "x", "z", timeout);
        insert_range(&mut pending_delete_ranges, id + 1, "f", "i", timeout);
        insert_range(&mut pending_delete_ranges, id + 1, "p", "t", timeout);
        assert_eq!(pending_delete_ranges.len(), 5);

        thread::sleep(delay / 2);

        //  a____c    f____i    m____n    p____t    x____z
        //              g___________________q
        // when we want to insert [g, q), we first extract overlap ranges,
        // which are [f, i), [m, n), [p, t)
        let timeout = time::Instant::now() + delay;
        let overlap_ranges =
            pending_delete_ranges.drain_overlap_ranges(&b"g".to_vec(), &b"q".to_vec());
        assert_eq!(
            overlap_ranges,
            [
                (id + 1, b"f".to_vec(), b"i".to_vec()),
                (id, b"m".to_vec(), b"n".to_vec()),
                (id + 1, b"p".to_vec(), b"t".to_vec()),
            ]
        );
        assert_eq!(pending_delete_ranges.len(), 2);
        insert_range(&mut pending_delete_ranges, id + 2, "g", "q", timeout);
        assert_eq!(pending_delete_ranges.len(), 3);

        thread::sleep(delay / 2);

        // at t1, [a, c) and [x, z) will timeout
        let now = time::Instant::now();
        let ranges: Vec<_> = pending_delete_ranges.timeout_ranges(now).collect();
        assert_eq!(
            ranges,
            [
                (id, b"a".to_vec(), b"c".to_vec()),
                (id, b"x".to_vec(), b"z".to_vec()),
            ]
        );
        for (_, start_key, _) in ranges {
            pending_delete_ranges.remove(&start_key);
        }
        assert_eq!(pending_delete_ranges.len(), 1);

        thread::sleep(delay / 2);

        // at t2, [g, q) will timeout
        let now = time::Instant::now();
        let ranges: Vec<_> = pending_delete_ranges.timeout_ranges(now).collect();
        assert_eq!(ranges, [(id + 2, b"g".to_vec(), b"q".to_vec())]);
        for (_, start_key, _) in ranges {
            pending_delete_ranges.remove(&start_key);
        }
        assert_eq!(pending_delete_ranges.len(), 0);
    }

    #[test]
    fn test_pending_applies() {
        let temp_dir = Builder::new()
            .prefix("test_pending_applies")
            .tempdir()
            .unwrap();
        let mut cf_opts = ColumnFamilyOptions::new();
        cf_opts.set_level_zero_slowdown_writes_trigger(5);
        cf_opts.set_disable_auto_compactions(true);
        let kv_cfs_opts = vec![
            rocks::util::CFOptions::new("default", cf_opts.clone()),
            rocks::util::CFOptions::new("write", cf_opts.clone()),
            rocks::util::CFOptions::new("lock", cf_opts.clone()),
            rocks::util::CFOptions::new("raft", cf_opts.clone()),
        ];
        let raft_cfs_opt = rocks::util::CFOptions::new(CF_DEFAULT, cf_opts);
        let engine = get_test_db_for_regions(
            &temp_dir,
            None,
            Some(raft_cfs_opt),
            None,
            Some(kv_cfs_opts),
            &[1, 2, 3, 4, 5, 6],
        )
        .unwrap();

        for cf_name in engine.kv.cf_names() {
            let cf = engine.kv.cf_handle(cf_name).unwrap();
            for i in 0..6 {
                engine.kv.put_cf(cf, &[i], &[i]).unwrap();
                engine.kv.put_cf(cf, &[i + 1], &[i + 1]).unwrap();
                engine.kv.flush_cf(cf, true).unwrap();
                // check level 0 files
                assert_eq!(
                    engine_rocks::util::get_cf_num_files_at_level(&engine.kv, cf, 0).unwrap(),
                    u64::from(i) + 1
                );
            }
        }

        let shared_block_cache = false;
        let engines = Engines::new(
            Arc::clone(&engine.kv),
            Arc::clone(&engine.raft),
            shared_block_cache,
        );
        let snap_dir = Builder::new().prefix("snap_dir").tempdir().unwrap();
        let mgr = SnapManager::new(snap_dir.path().to_str().unwrap(), None);
        let mut worker = Worker::new("snap-manager");
        let sched = worker.scheduler();
        let (router, receiver) = mpsc::sync_channel(1);
        let runner = RegionRunner::new(
            engines.clone(),
            mgr,
            0,
            true,
            Duration::from_secs(0),
            CoprocessorHost::default(),
            router,
        );
        let mut timer = Timer::new(1);
        timer.add_task(Duration::from_millis(100), Event::CheckApply);
        worker.start_with_timer(runner, timer).unwrap();

        let gen_and_apply_snap = |id: u64| {
            // construct snapshot
            let (tx, rx) = mpsc::sync_channel(1);
            let apply_state: RaftApplyState = engines
                .kv
                .c()
                .get_msg_cf(CF_RAFT, &keys::apply_state_key(id))
                .unwrap()
                .unwrap();
            let idx = apply_state.get_applied_index();
            let entry = engines
                .raft
                .c()
                .get_msg::<Entry>(&keys::raft_log_key(id, idx))
                .unwrap()
                .unwrap();
            sched
                .schedule(Task::Gen {
                    region_id: id,
                    kv_snap: RocksSnapshot::new(engines.kv.clone()),
                    last_applied_index_term: entry.get_term(),
                    last_applied_state: apply_state,
                    notifier: tx,
                })
                .unwrap();
            let s1 = rx.recv().unwrap();
            match receiver.recv() {
                Ok((region_id, CasualMessage::SnapshotGenerated)) => {
                    assert_eq!(region_id, id);
                }
                msg => panic!("expected SnapshotGenerated, but got {:?}", msg),
            }
            let data = s1.get_data();
            let key = SnapKey::from_snap(&s1).unwrap();
            let mgr = SnapManager::new(snap_dir.path().to_str().unwrap(), None);
            let mut s2 = mgr.get_snapshot_for_sending(&key).unwrap();
            let mut s3 = mgr.get_snapshot_for_receiving(&key, &data[..]).unwrap();
            io::copy(&mut s2, &mut s3).unwrap();
            s3.save().unwrap();

            // set applying state
            let mut wb = engine.kv.c().write_batch();
            let region_key = keys::region_state_key(id);
            let mut region_state = engine
                .kv
                .c()
                .get_msg_cf::<RegionLocalState>(CF_RAFT, &region_key)
                .unwrap()
                .unwrap();
            region_state.set_state(PeerState::Applying);
            wb.put_msg_cf(CF_RAFT, &region_key, &region_state).unwrap();
            engine.kv.c().write(&wb).unwrap();

            // apply snapshot
            let status = Arc::new(AtomicUsize::new(JOB_STATUS_PENDING));
            sched
                .schedule(Task::Apply {
                    region_id: id,
                    status,
                })
                .unwrap();
        };
        let wait_apply_finish = |id: u64| {
            let region_key = keys::region_state_key(id);
            loop {
                thread::sleep(Duration::from_millis(100));
                if engine
                    .kv
                    .c()
                    .get_msg_cf::<RegionLocalState>(CF_RAFT, &region_key)
                    .unwrap()
                    .unwrap()
                    .get_state()
                    == PeerState::Normal
                {
                    break;
                }
            }
        };
        let cf = engine.kv.cf_handle(CF_DEFAULT).unwrap();

        // snapshot will not ingest cause already write stall
        gen_and_apply_snap(1);
        assert_eq!(
            engine_rocks::util::get_cf_num_files_at_level(&engine.kv, cf, 0).unwrap(),
            6
        );

        // compact all files to the bottomest level
        rocks::util::compact_files_in_range(&engine.kv, None, None, None).unwrap();
        assert_eq!(
            engine_rocks::util::get_cf_num_files_at_level(&engine.kv, cf, 0).unwrap(),
            0
        );

        wait_apply_finish(1);

        // the pending apply task should be finished and snapshots are ingested.
        // note that when ingest sst, it may flush memtable if overlap,
        // so here will two level 0 files.
        assert_eq!(
            engine_rocks::util::get_cf_num_files_at_level(&engine.kv, cf, 0).unwrap(),
            2
        );

        // no write stall, ingest without delay
        gen_and_apply_snap(2);
        wait_apply_finish(2);
        assert_eq!(
            engine_rocks::util::get_cf_num_files_at_level(&engine.kv, cf, 0).unwrap(),
            4
        );

        // snapshot will not ingest cause it may cause write stall
        gen_and_apply_snap(3);
        assert_eq!(
            engine_rocks::util::get_cf_num_files_at_level(&engine.kv, cf, 0).unwrap(),
            4
        );
        gen_and_apply_snap(4);
        assert_eq!(
            engine_rocks::util::get_cf_num_files_at_level(&engine.kv, cf, 0).unwrap(),
            4
        );
        gen_and_apply_snap(5);
        assert_eq!(
            engine_rocks::util::get_cf_num_files_at_level(&engine.kv, cf, 0).unwrap(),
            4
        );

        // compact all files to the bottomest level
        rocks::util::compact_files_in_range(&engine.kv, None, None, None).unwrap();
        assert_eq!(
            engine_rocks::util::get_cf_num_files_at_level(&engine.kv, cf, 0).unwrap(),
            0
        );

        // make sure have checked pending applies
        wait_apply_finish(4);

        // before two pending apply tasks should be finished and snapshots are ingested
        // and one still in pending.
        assert_eq!(
            engine_rocks::util::get_cf_num_files_at_level(&engine.kv, cf, 0).unwrap(),
            4
        );

        // make sure have checked pending applies
        rocks::util::compact_files_in_range(&engine.kv, None, None, None).unwrap();
        assert_eq!(
            engine_rocks::util::get_cf_num_files_at_level(&engine.kv, cf, 0).unwrap(),
            0
        );
        wait_apply_finish(5);

        // the last one pending task finished
        assert_eq!(
            engine_rocks::util::get_cf_num_files_at_level(&engine.kv, cf, 0).unwrap(),
            2
        );
    }
}
