use std::collections::VecDeque;
use std::ops::Range;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use time::OffsetDateTime;

use crate::gpu;
use crate::mining::{Attempt, Hash256, Work};

pub const NONCE_SPACE: u64 = u32::MAX as u64 + 1;
const CPU_BATCH_SIZE: u64 = 16_384;
const MAX_PENDING_SOLUTIONS: usize = 4_096;
const GPU_BURST_TIME: Duration = Duration::from_millis(20);

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Backend {
    #[default]
    Cpu,
    Vulkan(usize),
}

#[derive(Clone, Default)]
pub struct GpuInfo {
    pub adapters: Option<Arc<[String]>>,
    pub status: GpuStatus,
    pub intensity: gpu::Intensity,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub enum GpuStatus {
    #[default]
    Idle,
    Preparing,
    Ready,
    Failed(String),
}

#[derive(Default)]
struct Scheduler {
    work: Option<Arc<Work>>,
    next_nonce: u64,
    best_limit: Option<Hash256>,
    pending: Option<Batch>,
    in_flight: usize,
    thread_count: usize,
    backend: Backend,
    gpu: GpuInfo,
    running: bool,
    stopping: bool,
}

impl Scheduler {
    fn exhausted(&self) -> bool {
        self.next_nonce == NONCE_SPACE && self.in_flight == 0
    }

    fn backpressured(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|batch| batch.solutions.len() >= MAX_PENDING_SOLUTIONS)
    }

    fn can_scan(&self) -> bool {
        self.running && self.next_nonce < NONCE_SPACE && !self.backpressured()
    }

    fn reserve(&mut self, batch_size: u64) -> (Arc<Work>, Range<u64>, Option<Hash256>) {
        let work = self
            .work
            .as_ref()
            .expect("Running workers have a job")
            .clone();
        let start = self.next_nonce;
        let end = (start + batch_size).min(NONCE_SPACE);
        self.next_nonce = end;
        self.in_flight += 1;
        (work, start..end, self.best_limit)
    }

    fn finish(&mut self, batch: Batch) {
        if let Some((best, _)) = &batch.best
            && self.best_limit.is_none_or(|limit| best.hash < limit)
        {
            self.best_limit = Some(best.hash);
        }
        if let Some(pending) = &mut self.pending {
            pending.merge(batch);
        } else {
            self.pending = Some(batch);
        }
    }
}

#[derive(Default)]
struct Shared {
    scheduler: Mutex<Scheduler>,
    changed: Condvar,
}

pub struct Workers {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    gpu_thread: Option<JoinHandle<()>>,
}

impl Workers {
    pub fn new(count: usize, intensity: gpu::Intensity) -> Result<Self> {
        let mut pool = Self {
            shared: Arc::new(Shared::default()),
            threads: Vec::new(),
            gpu_thread: None,
        };
        pool.set_gpu_intensity(intensity);
        pool.resize(count)?;
        let shared = pool.shared.clone();
        match thread::Builder::new()
            .name("vulkan-mining-worker".into())
            .spawn(move || run_gpu_worker(shared))
        {
            Ok(worker) => pool.gpu_thread = Some(worker),
            Err(error) => {
                let mut state = pool.shared.scheduler.lock().unwrap();
                state.gpu.adapters = Some(Arc::new([]));
                state.gpu.status = GpuStatus::Failed(error.to_string());
            }
        }
        Ok(pool)
    }

    pub fn resize(&mut self, count: usize) -> Result<()> {
        for index in self.threads.len()..count {
            let shared = self.shared.clone();
            let worker = thread::Builder::new()
                .name(format!("mining-worker-{}", index + 1))
                .spawn(move || run_cpu_worker(shared, index))
                .with_context(|| format!("Could not start mining worker {}", index + 1))?;
            self.threads.push(worker);
        }
        // Workers above the limit finish their current range, then sleep.
        self.shared.scheduler.lock().unwrap().thread_count = count;
        self.shared.changed.notify_all();
        Ok(())
    }

    pub fn gpu_info(&self) -> GpuInfo {
        self.shared.scheduler.lock().unwrap().gpu.clone()
    }

    pub fn set_gpu_intensity(&self, intensity: gpu::Intensity) {
        self.shared.scheduler.lock().unwrap().gpu.intensity = intensity;
        self.shared.changed.notify_all();
    }

    pub fn select_backend(&self, backend: Backend) {
        let mut state = self.shared.scheduler.lock().unwrap();
        debug_assert!(!state.running && state.in_flight == 0);
        state.backend = backend;
        state.gpu.status = match backend {
            Backend::Cpu => GpuStatus::Idle,
            Backend::Vulkan(_) => GpuStatus::Preparing,
        };
        self.shared.changed.notify_all();
    }

    pub fn install(&self, work: Arc<Work>, best_limit: Option<Hash256>) {
        let mut state = self.shared.scheduler.lock().unwrap();
        debug_assert!(state.in_flight == 0 && state.pending.is_none());
        state.running = false;
        state.work = Some(work);
        state.next_nonce = 0;
        state.best_limit = best_limit;
    }

    pub fn resume(&self, best_limit: Option<Hash256>) {
        let mut state = self.shared.scheduler.lock().unwrap();
        if let Some(limit) = best_limit {
            state.best_limit = Some(state.best_limit.map_or(limit, |current| current.min(limit)));
        }
        state.running = true;
        self.shared.changed.notify_all();
    }

    pub fn pause(&self) -> Option<Batch> {
        let mut state = self.shared.scheduler.lock().unwrap();
        state.running = false;
        let mut state = self
            .shared
            .changed
            .wait_while(state, |state| state.in_flight > 0)
            .unwrap();
        state.pending.take()
    }

    pub fn drain(&self) -> (Option<Batch>, bool) {
        let mut state = self.shared.scheduler.lock().unwrap();
        let was_full = state.backpressured();
        let report = state.pending.take();
        if was_full {
            self.shared.changed.notify_all();
        }
        (report, state.exhausted())
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        self.shared.scheduler.lock().unwrap().stopping = true;
        self.shared.changed.notify_all();
        for worker in self.threads.drain(..) {
            let _ = worker.join();
        }
        if let Some(worker) = self.gpu_thread.take() {
            let _ = worker.join();
        }
    }
}

fn run_cpu_worker(shared: Arc<Shared>, index: usize) {
    let mut state = shared.scheduler.lock().unwrap();
    loop {
        if state.stopping {
            break;
        }
        if state.backend != Backend::Cpu || index >= state.thread_count || !state.can_scan() {
            state = shared.changed.wait(state).unwrap();
            continue;
        }

        let (work, range, best_limit) = state.reserve(CPU_BATCH_SIZE);
        drop(state);

        let batch = scan(&work, range, best_limit);
        state = shared.scheduler.lock().unwrap();
        state.finish(batch);
        state.in_flight -= 1;
        if state.in_flight == 0 && (!state.running || state.exhausted()) {
            shared.changed.notify_all();
        }
    }
}

#[derive(Default)]
struct GpuPacing {
    intensity: gpu::Intensity,
    burst_started: Option<Instant>,
    idle_until: Option<Instant>,
}

impl GpuPacing {
    fn set_intensity(&mut self, intensity: gpu::Intensity, has_work_in_flight: bool) {
        if self.intensity != intensity {
            self.intensity = intensity;
            self.idle_until = None;
            self.burst_started =
                (intensity != gpu::Intensity::FULL && has_work_in_flight).then(Instant::now);
        }
    }

    fn can_submit(&self, now: Instant) -> bool {
        self.burst_started
            .is_none_or(|started| now - started < GPU_BURST_TIME)
    }

    fn begin_submission(&mut self) {
        if self.intensity != gpu::Intensity::FULL {
            self.burst_started.get_or_insert_with(Instant::now);
        }
    }

    fn finish_burst(&mut self, finished: Instant) {
        if let Some(started) = self.burst_started.take() {
            self.idle_until = Some(finished + self.intensity.idle_after(finished - started));
        }
    }

    fn idle_remaining(&self) -> Duration {
        self.idle_until.map_or(Duration::ZERO, |until| {
            until.saturating_duration_since(Instant::now())
        })
    }
}

fn run_gpu_worker(shared: Arc<Shared>) {
    let adapters = gpu::adapters();
    let mut state = shared.scheduler.lock().unwrap();
    state.gpu.adapters = Some(
        adapters
            .iter()
            .map(|adapter| adapter.get_info().name)
            .collect(),
    );
    let mut miner = None;
    let mut dispatched = VecDeque::<gpu::Dispatch>::with_capacity(gpu::SLOT_COUNT);
    let mut next_slot = 0;
    let mut pacing = GpuPacing::default();
    while !state.stopping {
        let Backend::Vulkan(index) = state.backend else {
            state = shared.changed.wait(state).unwrap();
            continue;
        };
        if state.gpu.status == GpuStatus::Preparing {
            drop(state);
            let result = match miner.take() {
                Some((previous, miner)) if previous == index => Ok(miner),
                _ => gpu::Miner::new(&adapters[index]),
            };
            state = shared.scheduler.lock().unwrap();
            if state.backend == Backend::Vulkan(index) {
                state.gpu.status = match result {
                    Ok(device) => {
                        miner = Some((index, device));
                        GpuStatus::Ready
                    }
                    Err(error) => GpuStatus::Failed(format!("{error:#}")),
                };
            }
            continue;
        }
        if state.gpu.status != GpuStatus::Ready {
            state = shared.changed.wait(state).unwrap();
            continue;
        }

        let (_, device) = miner.as_mut().expect("A ready GPU has a compute device");
        pacing.set_intensity(state.gpu.intensity, !dispatched.is_empty());
        let idle = pacing.idle_remaining();
        let can_scan = state.can_scan();
        let (unconfirmed_start, result) = if can_scan
            && idle.is_zero()
            && dispatched.len() < gpu::SLOT_COUNT
            && pacing.can_submit(Instant::now())
        {
            let work = state.work.as_deref().expect("Running workers have a job");
            let size = gpu::batch_size(work, state.best_limit);
            let (work, range, best_limit) = state.reserve(size);
            drop(state);
            pacing.begin_submission();
            let start = dispatched.front().map_or(range.start, gpu::Dispatch::start);
            let result = device
                .submit(next_slot, work, range, best_limit)
                .map(|dispatch| {
                    dispatched.push_back(dispatch);
                    next_slot = (next_slot + 1) % gpu::SLOT_COUNT;
                    None
                });
            (start, result)
        } else if let Some(dispatch) = dispatched.pop_front() {
            drop(state);
            (dispatch.start(), device.collect(dispatch).map(Some))
        } else {
            state = if can_scan && !idle.is_zero() {
                shared.changed.wait_timeout(state, idle).unwrap().0
            } else {
                shared.changed.wait(state).unwrap()
            };
            continue;
        };
        state = shared.scheduler.lock().unwrap();
        match result {
            Ok(None) => continue,
            Ok(Some(batch)) => {
                state.finish(batch);
                state.in_flight -= 1;
                if dispatched.is_empty() {
                    pacing.finish_burst(Instant::now());
                }
            }
            Err(error) => {
                // Results are collected in nonce order, so only the unconfirmed suffix is retried.
                state.next_nonce = unconfirmed_start;
                state.in_flight = 0;
                state.running = false;
                state.gpu.status = GpuStatus::Failed(format!("{error:#}"));
                shared.changed.notify_all();
                drop(state);
                dispatched.clear();
                next_slot = 0;
                pacing = GpuPacing::default();
                miner = None;
                state = shared.scheduler.lock().unwrap();
            }
        }
        if state.in_flight == 0 && (!state.running || state.exhausted()) {
            shared.changed.notify_all();
        }
    }
}

pub struct Batch {
    pub count: u64,
    pub last: Attempt,
    pub best: Option<(Attempt, OffsetDateTime)>,
    pub solutions: Vec<(u32, Hash256)>,
    pub(crate) finished: Instant,
}

impl Batch {
    fn merge(&mut self, mut other: Self) {
        self.count += other.count;
        if other.finished > self.finished {
            self.last = other.last;
            self.finished = other.finished;
        }
        if let Some((attempt, timestamp)) = other.best
            && self
                .best
                .as_ref()
                .is_none_or(|(best, _)| attempt.hash < best.hash)
        {
            self.best = Some((attempt, timestamp));
        }
        self.solutions.append(&mut other.solutions);
    }
}

fn scan(work: &Work, range: Range<u64>, mut best_limit: Option<Hash256>) -> Batch {
    let count = range.end - range.start;
    let last_nonce = (range.end - 1) as u32;
    let target = work.job.submission_target();
    let mut best = None;
    let mut solutions = Vec::new();
    let mut nonces = range.map(|nonce| nonce as u32);
    let last = loop {
        let nonce = nonces
            .next()
            .expect("Mining ranges contain at least one nonce");
        let result = work.hash_nonce(nonce);
        if result.hash <= target {
            solutions.push((nonce, result.hash));
        }
        if best_limit.as_ref().is_none_or(|best| result.hash < *best) {
            best_limit = Some(result.hash);
            best = Some((work.attempt(nonce, result), OffsetDateTime::now_utc()));
        }
        if nonce == last_nonce {
            break work.attempt(nonce, result);
        }
    };
    Batch {
        count,
        last,
        best,
        solutions,
        finished: Instant::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::Network;
    use bitcoin::blockdata::constants::genesis_block;
    use bitcoin::consensus::encode::serialize;
    use serde_json::json;

    use crate::mining::Job;

    #[test]
    fn gpu_pacing_limits_bursts_and_cancels_idle_when_intensity_changes() {
        let mut pacing = GpuPacing::default();
        let quarter = gpu::Intensity::new(25).unwrap();
        pacing.set_intensity(quarter, false);
        pacing.begin_submission();
        let started = pacing.burst_started.unwrap();
        let active = GPU_BURST_TIME;
        assert!(pacing.can_submit(started + active / 2));
        assert!(!pacing.can_submit(started + active));
        pacing.finish_burst(started + active);
        assert!(pacing.can_submit(started + active));
        assert_eq!(pacing.idle_until, Some(started + active * 4));

        pacing.set_intensity(gpu::Intensity::FULL, false);
        assert!(pacing.idle_remaining().is_zero());
        pacing.begin_submission();
        assert!(pacing.burst_started.is_none());

        pacing.set_intensity(quarter, true);
        assert!(pacing.burst_started.is_some());
    }

    #[test]
    fn independent_workers_preserve_nonce_coverage_across_pause_and_resize() -> Result<()> {
        let block = genesis_block(Network::Bitcoin);
        let coinbase = &block.txdata[0];
        let share = ("test".into(), [255; 32]);
        let job = Job::parse(
            &json!([
                "test",
                "00".repeat(32),
                hex::encode(serialize(coinbase)),
                "",
                [],
                "00000001",
                "1d00ffff",
                "495fab29",
                true,
            ]),
            Some(&share),
        )?;
        let work = Arc::new(Work::new(
            job,
            &[],
            vec![],
            &coinbase.output[0].script_pubkey,
        )?);
        let mut workers = Workers::new(1, gpu::Intensity::FULL)?;
        let start = NONCE_SPACE - 6 * CPU_BATCH_SIZE - 3;
        workers.install(work.clone(), None);
        workers.shared.scheduler.lock().unwrap().next_nonce = start;
        workers.resume(None);

        let (mut combined, _) = wait_for_report(&workers)?;
        if let Some(batch) = workers.pause() {
            combined.merge(batch);
        }
        let paused_at = workers.shared.scheduler.lock().unwrap().next_nonce;
        assert!(paused_at > start && paused_at < NONCE_SPACE);

        workers.resize(3)?;
        assert!(workers.drain().0.is_none());
        assert_eq!(
            workers.shared.scheduler.lock().unwrap().next_nonce,
            paused_at
        );
        workers.resume(None);
        let (batch, mut exhausted) = wait_for_report(&workers)?;
        combined.merge(batch);
        workers.resize(1)?;
        while !exhausted {
            let (batch, done) = wait_for_report(&workers)?;
            combined.merge(batch);
            exhausted = done;
        }

        assert_eq!(combined.count, NONCE_SPACE - start);
        combined.solutions.sort_unstable_by_key(|(nonce, _)| *nonce);
        assert!(
            combined
                .solutions
                .iter()
                .map(|(nonce, _)| u64::from(*nonce))
                .eq(start..NONCE_SPACE)
        );
        assert_eq!(
            combined.solutions.last().unwrap().1,
            work.try_nonce(u32::MAX).hash
        );
        assert_eq!(
            combined.best.unwrap().0.hash,
            combined
                .solutions
                .iter()
                .map(|(_, hash)| *hash)
                .min()
                .unwrap()
        );

        assert!(workers.pause().is_none());
        let mut new_job = work.job.clone();
        new_job.time += 1;
        let new_work = Arc::new(Work::new(
            new_job,
            &[],
            vec![],
            &coinbase.output[0].script_pubkey,
        )?);
        workers.install(new_work.clone(), None);
        assert_eq!(workers.shared.scheduler.lock().unwrap().next_nonce, 0);
        workers.resume(None);
        let (fresh, _) = wait_for_report(&workers)?;
        assert_eq!(fresh.last.header[..76], new_work.header()[..76]);
        Ok(())
    }

    fn wait_for_report(workers: &Workers) -> Result<(Batch, bool)> {
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while Instant::now() < deadline {
            let (batch, exhausted) = workers.drain();
            if let Some(batch) = batch {
                return Ok((batch, exhausted));
            }
            thread::sleep(std::time::Duration::from_millis(1));
        }
        anyhow::bail!("Timed out waiting for mining workers")
    }
}
