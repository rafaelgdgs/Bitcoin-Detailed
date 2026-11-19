use std::ops::Range;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use anyhow::{Context, Result};
use time::OffsetDateTime;

use crate::mining::{Attempt, Hash256, Work};

pub const NONCE_SPACE: u64 = u32::MAX as u64 + 1;
const BATCH_SIZE: u64 = 16_384;

struct Task {
    work: Arc<Work>,
    range: Range<u64>,
    best_limit: Option<Hash256>,
}

struct Worker {
    tasks: mpsc::Sender<Task>,
    results: mpsc::Receiver<Batch>,
}

pub struct CpuWorkers {
    workers: Vec<Worker>,
    threads: Vec<JoinHandle<()>>,
}

impl CpuWorkers {
    pub fn new(count: usize) -> Result<Self> {
        let mut pool = Self {
            workers: Vec::new(),
            threads: Vec::new(),
        };
        // The coordinator hashes one range itself, so one configured thread stays inline.
        for number in 1..count {
            let (tasks, pending) = mpsc::channel::<Task>();
            let (finished, results) = mpsc::channel();
            let thread = thread::Builder::new()
                .name(format!("mining-worker-{number}"))
                .spawn(move || {
                    while let Ok(task) = pending.recv() {
                        if finished
                            .send(scan(&task.work, task.range, task.best_limit))
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .with_context(|| format!("Could not start mining worker {number}"))?;
            pool.workers.push(Worker { tasks, results });
            pool.threads.push(thread);
        }
        Ok(pool)
    }

    pub fn scan(
        &self,
        work: &Arc<Work>,
        next_nonce: &mut u64,
        best_limit: Option<Hash256>,
    ) -> Result<Batch> {
        let own_range = take_range(next_nonce);
        let mut dispatched = 0;
        for worker in &self.workers {
            let range = take_range(next_nonce);
            if range.is_empty() {
                break;
            }
            worker
                .tasks
                .send(Task {
                    work: work.clone(),
                    range,
                    best_limit,
                })
                .context("Mining worker stopped")?;
            dispatched += 1;
        }

        let mut batch = scan(work, own_range, best_limit);
        for worker in &self.workers[..dispatched] {
            batch.merge(worker.results.recv().context("Mining worker stopped")?);
        }
        Ok(batch)
    }
}

impl Drop for CpuWorkers {
    fn drop(&mut self) {
        self.workers.clear();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
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

fn take_range(next_nonce: &mut u64) -> Range<u64> {
    let start = *next_nonce;
    *next_nonce = (start + BATCH_SIZE).min(NONCE_SPACE);
    start..*next_nonce
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
    fn workers_cover_disjoint_ranges_without_wrapping_at_the_last_nonce() -> Result<()> {
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
        let workers = CpuWorkers::new(3)?;
        let start = NONCE_SPACE - 2 * BATCH_SIZE - 3;
        let mut next_nonce = start;
        let batch = workers.scan(&work, &mut next_nonce, None)?;
        assert_eq!(next_nonce, NONCE_SPACE);
        assert_eq!(batch.count, NONCE_SPACE - start);
        assert!(
            batch
                .solutions
                .iter()
                .map(|(nonce, _)| u64::from(*nonce))
                .eq(start..NONCE_SPACE)
        );
        assert_eq!(
            batch.solutions.last().unwrap().1,
            work.try_nonce(u32::MAX).hash
        );
        assert_eq!(
            batch.best.unwrap().0.hash,
            batch.solutions.iter().map(|(_, hash)| *hash).min().unwrap()
        );
        Ok(())
    }
}
