use std::ops::Range;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use anyhow::{Context, Result};
use time::OffsetDateTime;

use crate::mining::{Attempt, Hash256, Work};

pub const NONCE_SPACE: u64 = u32::MAX as u64 + 1;
const BATCH_SIZE: u64 = 16_384;
const MAX_PENDING_SOLUTIONS: usize = 4_096;

#[derive(Default)]
struct Scheduler {
    work: Option<Arc<Work>>,
    next_nonce: u64,
    best_limit: Option<Hash256>,
    pending: Option<Batch>,
    in_flight: usize,
    thread_count: usize,
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
}

#[derive(Default)]
struct Shared {
    scheduler: Mutex<Scheduler>,
    changed: Condvar,
}

pub struct CpuWorkers {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl CpuWorkers {
    pub fn new(count: usize) -> Result<Self> {
        let mut pool = Self {
            shared: Arc::new(Shared::default()),
            threads: Vec::new(),
        };
        pool.resize(count)?;
        Ok(pool)
    }

    pub fn resize(&mut self, count: usize) -> Result<()> {
        for index in self.threads.len()..count {
            let shared = self.shared.clone();
            let worker = thread::Builder::new()
                .name(format!("mining-worker-{}", index + 1))
                .spawn(move || run_worker(shared, index))
                .with_context(|| format!("Could not start mining worker {}", index + 1))?;
            self.threads.push(worker);
        }
        // Workers above the limit finish their current range, then sleep.
        self.shared.scheduler.lock().unwrap().thread_count = count;
        self.shared.changed.notify_all();
        Ok(())
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

impl Drop for CpuWorkers {
    fn drop(&mut self) {
        self.shared.scheduler.lock().unwrap().stopping = true;
        self.shared.changed.notify_all();
        for worker in self.threads.drain(..) {
            let _ = worker.join();
        }
    }
}

fn run_worker(shared: Arc<Shared>, index: usize) {
    let mut state = shared.scheduler.lock().unwrap();
    loop {
        if state.stopping {
            break;
        }
        if !state.running
            || index >= state.thread_count
            || state.next_nonce == NONCE_SPACE
            || state.backpressured()
        {
            state = shared.changed.wait(state).unwrap();
            continue;
        }

        let work = state
            .work
            .as_ref()
            .expect("Running workers have a job")
            .clone();
        let best_limit = state.best_limit;
        let start = state.next_nonce;
        let end = (start + BATCH_SIZE).min(NONCE_SPACE);
        state.next_nonce = end;
        state.in_flight += 1;
        drop(state);

        let batch = scan(&work, start..end, best_limit);
        state = shared.scheduler.lock().unwrap();
        if let Some((best, _)) = &batch.best
            && state
                .best_limit
                .as_ref()
                .is_none_or(|limit| best.hash < *limit)
        {
            state.best_limit = Some(best.hash);
        }
        if let Some(pending) = &mut state.pending {
            pending.merge(batch);
        } else {
            state.pending = Some(batch);
        }
        state.in_flight -= 1;
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
    finished: Instant,
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
        let mut workers = CpuWorkers::new(1)?;
        let start = NONCE_SPACE - 6 * BATCH_SIZE - 3;
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

    fn wait_for_report(workers: &CpuWorkers) -> Result<(Batch, bool)> {
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
