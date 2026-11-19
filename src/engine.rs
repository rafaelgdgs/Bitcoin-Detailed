use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use bitcoin::address::NetworkUnchecked;
use bitcoin::{Address, Network, Txid};
use serde_json::{Value, json};
use std::str::FromStr;
use time::OffsetDateTime;

use crate::mining::{self, Attempt, Hash256, Job, Work};
use crate::storage::{Best, Storage};
use crate::stratum::Stratum;
use crate::workers::{Backend, Batch, GpuInfo, GpuStatus, Workers};

type PendingShares = HashMap<u64, (bool, String)>;

pub fn max_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |count| count.get())
}

pub fn parse_thread_count(input: &str) -> Result<usize> {
    let max = max_threads();
    input
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|count| (1..=max).contains(count))
        .with_context(|| format!("Enter a CPU thread count from 1 to {max}"))
}

pub struct Config {
    pub endpoint: String,
    pub address: Address,
    pub user: String,
    pub password: String,
    pub threads: usize,
}

impl Config {
    pub fn new(
        endpoint: &str,
        address: &str,
        worker: &str,
        password: &str,
        threads: &str,
    ) -> Result<Self> {
        let address = Address::<NetworkUnchecked>::from_str(address.trim())
            .context("Invalid Bitcoin address")?
            .require_network(Network::Bitcoin)
            .context("Use a Bitcoin mainnet address")?;
        let endpoint = endpoint
            .trim()
            .strip_prefix("stratum+tcp://")
            .unwrap_or(endpoint.trim());
        let (host, port) = endpoint
            .rsplit_once(':')
            .context("Enter the pool as host:port")?;
        ensure!(
            !host.is_empty()
                && !host.contains(['/', '@'])
                && !host.chars().any(char::is_whitespace)
                && port.parse::<u16>().is_ok_and(|p| p > 0),
            "Enter a Stratum V1 TCP endpoint as host:port"
        );
        let worker = worker.trim();
        ensure!(
            worker
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)),
            "The worker name can contain letters, numbers, _ and -"
        );
        let user = if worker.is_empty() {
            address.to_string()
        } else {
            format!("{address}.{worker}")
        };
        let threads = parse_thread_count(threads)?;
        Ok(Self {
            endpoint: endpoint.to_owned(),
            address,
            user,
            password: password.to_owned(),
            threads,
        })
    }
}

pub enum Command {
    Toggle,
    SetThreads(usize),
    SelectBackend(Backend),
    Manual(Arc<Work>, u32),
    Txids(Arc<Work>),
    Stop,
}

#[derive(Clone)]
pub struct Snapshot {
    pub status: String,
    pub message: String,
    pub work: Option<Arc<Work>>,
    pub last: Option<Attempt>,
    pub best: Option<Best>,
    pub total: u64,
    pub rate: u64,
    pub running: bool,
    pub threads: usize,
    pub backend: Backend,
    pub gpu: GpuInfo,
    pub accepted: u64,
    pub rejected: u64,
    pub network_hits: u64,
    pub candidate: Option<String>,
    pub txids: Option<Arc<Vec<Txid>>>,
    pub tx_status: String,
}

impl Snapshot {
    fn new(best: Option<Best>, threads: usize) -> Self {
        Self {
            status: "Connecting…".into(),
            message: "Space starts or pauses automatic mining. N tests a nonce.".into(),
            work: None,
            last: None,
            best,
            total: 0,
            rate: 0,
            running: false,
            threads,
            backend: Backend::Cpu,
            gpu: GpuInfo::default(),
            accepted: 0,
            rejected: 0,
            network_hits: 0,
            candidate: None,
            txids: None,
            tx_status: "Press T to request the TXIDs for this job.".into(),
        }
    }
}

pub struct Connection {
    pub commands: mpsc::Sender<Command>,
    pub snapshot: Arc<Mutex<Snapshot>>,
    thread: std::thread::JoinHandle<()>,
}

impl Connection {
    pub fn stop(self) {
        let _ = self.commands.send(Command::Stop);
        let _ = self.thread.join();
    }
}

pub fn start(config: Config, storage: Storage, best: Option<Best>) -> Connection {
    let (sender, commands) = mpsc::channel();
    let snapshot = Snapshot::new(best, config.threads);
    let shared = Arc::new(Mutex::new(snapshot.clone()));
    let published = Arc::clone(&shared);
    let thread = std::thread::spawn(move || {
        let mut engine = Engine {
            config,
            commands,
            published,
            snapshot,
            storage,
            best_dirty: false,
            automatic: false,
            submitted: HashSet::new(),
        };
        engine.run();
    });
    Connection {
        commands: sender,
        snapshot: shared,
        thread,
    }
}

struct Engine {
    config: Config,
    commands: mpsc::Receiver<Command>,
    published: Arc<Mutex<Snapshot>>,
    snapshot: Snapshot,
    storage: Storage,
    best_dirty: bool,
    automatic: bool,
    submitted: HashSet<(Vec<u8>, u32)>,
}

impl Engine {
    fn publish(&self) {
        *self.published.lock().unwrap() = self.snapshot.clone();
    }

    fn save_best(&mut self) {
        if self.best_dirty
            && let Some(best) = &self.snapshot.best
        {
            match self.storage.save_best(best) {
                Ok(state) => {
                    self.best_dirty = false;
                    self.snapshot.best = state.best;
                }
                Err(error) => {
                    self.snapshot.message = format!("Could not save the best hash: {error:#}");
                }
            }
        }
    }

    fn set_threads(&mut self, count: usize, workers: &mut Workers) {
        if let Err(error) = workers.resize(count) {
            self.snapshot.message = format!("Could not change CPU threads: {error:#}");
            self.publish();
            return;
        }
        self.config.threads = count;
        self.snapshot.threads = count;
        self.snapshot.message = match self.storage.save_threads(count) {
            Ok(()) => format!("CPU threads set to {count}."),
            Err(error) => {
                format!("CPU threads set to {count}. Could not save the setting: {error:#}")
            }
        };
        self.publish();
    }

    fn select_backend(
        &mut self,
        backend: Backend,
        workers: &Workers,
        submission: Option<(&mut Stratum, &mut PendingShares)>,
    ) -> Result<()> {
        self.snapshot.gpu = workers.gpu_info();
        if backend == self.snapshot.backend
            && !matches!(self.snapshot.gpu.status, GpuStatus::Failed(_))
        {
            return Ok(());
        }
        if let Backend::Vulkan(index) = backend
            && self
                .snapshot
                .gpu
                .adapters
                .as_ref()
                .is_none_or(|adapters| index >= adapters.len())
        {
            self.snapshot.message = "Select a listed Vulkan GPU.".into();
            self.publish();
            return Ok(());
        }
        self.pause_workers(workers, submission)?;
        workers.select_backend(backend);
        self.snapshot.backend = backend;
        self.snapshot.rate = 0;
        self.snapshot.gpu = workers.gpu_info();
        self.snapshot.message = match backend {
            Backend::Cpu => "CPU mining selected.".into(),
            Backend::Vulkan(_) => "Vulkan GPU mining selected.".into(),
        };
        self.publish();
        Ok(())
    }

    fn refresh_gpu(&mut self, workers: &Workers) -> bool {
        self.snapshot.gpu = workers.gpu_info();
        match self.snapshot.backend {
            Backend::Cpu => true,
            Backend::Vulkan(_) => {
                if let GpuStatus::Failed(error) = &self.snapshot.gpu.status
                    && self.automatic
                {
                    self.automatic = false;
                    self.snapshot.message = format!("Vulkan mining paused: {error}");
                }
                self.snapshot.gpu.status == GpuStatus::Ready
            }
        }
    }

    fn run(&mut self) {
        let mut workers = match Workers::new(self.config.threads) {
            Ok(workers) => workers,
            Err(error) => {
                self.snapshot.status = format!("Could not start mining workers: {error:#}");
                self.publish();
                return;
            }
        };
        loop {
            if let Err(error) = self.session(&mut workers) {
                self.snapshot.status = format!("Disconnected · retrying in 5 s: {error:#}");
                self.snapshot.running = false;
                self.snapshot.work = None;
                self.snapshot.last = None;
                self.snapshot.rate = 0;
                self.snapshot.txids = None;
                self.save_best();
                self.publish();
            } else {
                self.save_best();
                return;
            }
            let until = Instant::now() + Duration::from_secs(5);
            while Instant::now() < until {
                let wait = until.saturating_duration_since(Instant::now());
                match self
                    .commands
                    .recv_timeout(wait.min(Duration::from_millis(100)))
                {
                    Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    Ok(Command::Toggle) => self.automatic = !self.automatic,
                    Ok(Command::SetThreads(count)) => self.set_threads(count, &mut workers),
                    Ok(Command::SelectBackend(backend)) => {
                        // No socket is active and the previous session already drained its work.
                        let _ = self.select_backend(backend, &workers, None);
                    }
                    Ok(_) => {}
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                self.refresh_gpu(&workers);
                self.publish();
            }
        }
    }

    fn session(&mut self, workers: &mut Workers) -> Result<()> {
        let mut socket = Stratum::connect(&self.config.endpoint)?;
        let result = self.run_session(&mut socket, workers);
        // Preserve work already computed if the connection failed during a range.
        self.pause_workers(workers, None)?;
        result
    }

    fn run_session(&mut self, socket: &mut Stratum, workers: &mut Workers) -> Result<()> {
        let subscribe = socket.call("mining.subscribe", json!(["bitcoin-detailed/0.1"]))?;
        let mut authorize = None;
        let mut authorized = false;
        let mut extranonce1 = Vec::new();
        let mut extranonce2_size = 0;
        let mut latest_job = None;
        let mut difficulty = None;
        let mut shares = HashMap::new();
        let mut tx_request = None;
        let mut last_read = Instant::now();
        let mut updated = Instant::now();
        let mut last_save = Instant::now();
        let mut total_at_update = self.snapshot.total;
        let payout = self.config.address.script_pubkey();
        self.snapshot.status = "Connected · negotiating Stratum V1".into();
        self.publish();

        loop {
            // Workers keep hashing while this thread waits for pool messages.
            let wait = Duration::from_millis(if self.snapshot.running { 2 } else { 10 });
            let messages = socket.read(wait);
            if !messages.is_empty() {
                last_read = Instant::now();
            }
            ensure!(
                last_read.elapsed()
                    < Duration::from_secs(if self.snapshot.work.is_some() {
                        180
                    } else {
                        20
                    }),
                "Timed out waiting for Stratum messages"
            );
            for message in messages {
                let message = message?;
                let id = message["id"].as_u64();
                let params = &message["params"];
                let result = &message["result"];
                let error = &message["error"];
                match message["method"].as_str() {
                    Some("mining.set_difficulty") => {
                        difficulty = Some(mining::share_target(&params[0])?)
                    }
                    Some("mining.notify") => {
                        let job = Job::parse(params, difficulty.as_ref())?;
                        if latest_job.as_ref() != Some(&job) {
                            self.pause_workers(
                                workers,
                                (!job.clean).then_some((&mut *socket, &mut shares)),
                            )?;
                            latest_job = Some(job.clone());
                            tx_request = None;
                            self.invalidate_work();
                            if authorized {
                                self.install_job(
                                    workers,
                                    job,
                                    &extranonce1,
                                    extranonce2_size,
                                    &payout,
                                )?;
                            }
                        }
                    }
                    Some("mining.set_extranonce") => {
                        (extranonce1, extranonce2_size) =
                            parse_extranonces(&params[0], &params[1])?;
                        self.pause_workers(workers, None)?;
                        tx_request = None;
                        self.invalidate_work();
                        if authorized && let Some(job) = latest_job.clone() {
                            self.install_job(
                                workers,
                                job,
                                &extranonce1,
                                extranonce2_size,
                                &payout,
                            )?;
                        }
                    }
                    Some("client.show_message") => {
                        if let Some(notice) = params[0].as_str() {
                            self.snapshot.message = format!("Pool: {notice}");
                        }
                    }
                    Some("mining.ping") => {
                        socket.reply(&message["id"], json!("pong"), Value::Null)?;
                    }
                    Some("client.get_version") => {
                        socket.reply(&message["id"], json!("bitcoin-detailed/0.1"), Value::Null)?;
                    }
                    Some("client.reconnect") => {
                        bail!("The pool requested a reconnect to the configured endpoint")
                    }
                    Some(_) => {}
                    None if id == Some(subscribe) => {
                        ensure!(error.is_null(), "mining.subscribe rejected: {error}");
                        (extranonce1, extranonce2_size) =
                            parse_extranonces(&result[1], &result[2])?;
                        authorize = Some(socket.call(
                            "mining.authorize",
                            json!([self.config.user, self.config.password]),
                        )?);
                    }
                    None if id.is_some() && id == authorize => {
                        ensure!(
                            result == true && error.is_null(),
                            "The pool rejected the address or worker: {error}"
                        );
                        authorized = true;
                        self.snapshot.status = "Authorized · waiting for a job".into();
                        if let Some(job) = latest_job.clone() {
                            self.install_job(
                                workers,
                                job,
                                &extranonce1,
                                extranonce2_size,
                                &payout,
                            )?;
                        }
                    }
                    None if tx_request
                        .as_ref()
                        .is_some_and(|(request, _, _)| Some(*request) == id) =>
                    {
                        let (_, job_id, _) = tx_request.take().unwrap();
                        if let Some(work) = self.snapshot.work.as_ref()
                            && work.job.id == job_id
                        {
                            self.snapshot.tx_status = match read_txids(work, result, error) {
                                Ok(txids) => {
                                    let count = txids.len() + 1;
                                    self.snapshot.txids = Some(Arc::new(txids));
                                    format!(
                                        "{count} transactions verified against the Merkle root and branch. Press Enter to look up the selection in Esplora."
                                    )
                                }
                                Err(error) => format!("TXIDs unavailable: {error}"),
                            };
                        }
                    }
                    None => {
                        if let Some((is_block, hash)) = id.and_then(|id| shares.remove(&id)) {
                            let message = if result == true && error.is_null() {
                                self.snapshot.accepted += 1;
                                if is_block {
                                    "Block candidate accepted by the pool. Check for network confirmation.".into()
                                } else {
                                    "Share accepted by the pool.".into()
                                }
                            } else {
                                self.snapshot.rejected += 1;
                                format!(
                                    "{} rejected: {error}",
                                    if is_block { "Block candidate" } else { "Share" }
                                )
                            };
                            if is_block {
                                self.snapshot.candidate = Some(format!("{message} {hash}"));
                            }
                            self.snapshot.message = message;
                        }
                    }
                }
            }
            if tx_request
                .as_ref()
                .is_some_and(|(_, _, sent): &(u64, String, Instant)| {
                    sent.elapsed() > Duration::from_secs(10)
                })
            {
                tx_request = None;
                self.snapshot.tx_status = "The pool did not respond to the TXID request.".into();
            }

            loop {
                match self.commands.try_recv() {
                    Ok(Command::Stop) | Err(mpsc::TryRecvError::Disconnected) => {
                        self.pause_workers(workers, Some((socket, &mut shares)))?;
                        return Ok(());
                    }
                    Ok(Command::Toggle) => {
                        self.automatic = !self.automatic;
                        self.snapshot.message = if self.automatic {
                            "Automatic mining started.".into()
                        } else {
                            "Mining paused.".into()
                        };
                    }
                    Ok(Command::SetThreads(count)) => self.set_threads(count, workers),
                    Ok(Command::SelectBackend(backend)) => {
                        let previous = self.snapshot.backend;
                        self.select_backend(backend, workers, Some((socket, &mut shares)))?;
                        if self.snapshot.backend != previous {
                            total_at_update = self.snapshot.total;
                            updated = Instant::now();
                        }
                    }
                    Ok(Command::Manual(shown, nonce)) => {
                        self.automatic = false;
                        self.pause_workers(workers, Some((socket, &mut shares)))?;
                        self.snapshot.message = "Manual attempt · mining paused.".into();
                        if self
                            .snapshot
                            .work
                            .as_ref()
                            .is_some_and(|work| Arc::ptr_eq(work, &shown))
                        {
                            let attempt = shown.try_nonce(nonce);
                            self.snapshot.last = Some(attempt);
                            self.snapshot.total += 1;
                            self.record_best(&shown, attempt, None);
                            if attempt.hash <= shown.job.submission_target() {
                                self.submit(&shown, nonce, attempt.hash, socket, &mut shares)?;
                            }
                        } else {
                            self.snapshot.message =
                                "The job changed. Press N to try a nonce on the new job.".into();
                        }
                    }
                    Ok(Command::Txids(shown)) => {
                        if tx_request.is_none()
                            && self.snapshot.txids.is_none()
                            && self
                                .snapshot
                                .work
                                .as_ref()
                                .is_some_and(|work| work.job.id == shown.job.id)
                        {
                            let id = socket.call("mining.get_txnhashes", json!([shown.job.id]))?;
                            tx_request = Some((id, shown.job.id.clone(), Instant::now()));
                            self.snapshot.tx_status = "Requesting mining.get_txnhashes…".into();
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }

            let backend_ready = self.refresh_gpu(workers);
            if self.automatic
                && backend_ready
                && let Some(work) = self.snapshot.work.clone().filter(|work| work.paid_sats > 0)
            {
                let (batch, exhausted) = workers.drain();
                if let Some(batch) = batch {
                    self.apply_batch(&work, batch, Some((socket, &mut shares)))?;
                }
                if exhausted {
                    self.snapshot.running = false;
                    let mut nonce2 = work.extranonce2.clone();
                    if mining::increment(&mut nonce2) {
                        let next =
                            Arc::new(Work::new(work.job.clone(), &extranonce1, nonce2, &payout)?);
                        workers.install(next.clone(), self.best_limit());
                        self.snapshot.work = Some(next);
                        self.snapshot.last = None;
                    } else {
                        self.automatic = false;
                        self.snapshot.message =
                            "Nonce space exhausted. Waiting for a new job.".into();
                    }
                }
                if self.automatic && !self.snapshot.running {
                    workers.resume(self.best_limit());
                    self.snapshot.running = true;
                }
            } else if self.snapshot.running {
                self.pause_workers(workers, Some((socket, &mut shares)))?;
            }

            let elapsed = updated.elapsed();
            if elapsed >= Duration::from_millis(100) {
                self.snapshot.rate =
                    ((self.snapshot.total - total_at_update) as f64 / elapsed.as_secs_f64()) as u64;
                total_at_update = self.snapshot.total;
                updated = Instant::now();
                self.publish();
            }
            if last_save.elapsed() >= Duration::from_secs(2) {
                self.save_best();
                last_save = Instant::now();
            }
        }
    }

    fn invalidate_work(&mut self) {
        self.snapshot.work = None;
        self.snapshot.last = None;
        self.snapshot.txids = None;
        self.snapshot.tx_status = "Press T to request the TXIDs for this job.".into();
        self.submitted.clear();
    }

    fn install_job(
        &mut self,
        workers: &Workers,
        job: Job,
        extranonce1: &[u8],
        len: usize,
        payout: &bitcoin::Script,
    ) -> Result<()> {
        let work = Arc::new(Work::new(job, extranonce1, vec![0; len], payout)?);
        self.snapshot.status = if work.paid_sats == 0 {
            "Job received · no payout output for your address".into()
        } else {
            "Mainnet job received · payout found in the coinbase".into()
        };
        workers.install(work.clone(), self.best_limit());
        self.snapshot.work = Some(work);
        Ok(())
    }

    fn best_limit(&self) -> Option<Hash256> {
        self.snapshot.best.as_ref().map(|best| best.hash)
    }

    fn pause_workers(
        &mut self,
        workers: &Workers,
        submission: Option<(&mut Stratum, &mut PendingShares)>,
    ) -> Result<()> {
        self.snapshot.running = false;
        if let Some(batch) = workers.pause()
            && let Some(work) = self.snapshot.work.clone()
        {
            self.apply_batch(&work, batch, submission)?;
        }
        Ok(())
    }

    fn apply_batch(
        &mut self,
        work: &Work,
        batch: Batch,
        submission: Option<(&mut Stratum, &mut PendingShares)>,
    ) -> Result<()> {
        self.snapshot.last = Some(batch.last);
        self.snapshot.total += batch.count;
        if let Some((attempt, timestamp)) = batch.best {
            self.record_best(work, attempt, Some(timestamp));
        }
        if let Some((socket, shares)) = submission {
            for (nonce, hash) in batch.solutions {
                self.submit(work, nonce, hash, socket, shares)?;
            }
        }
        Ok(())
    }

    fn record_best(&mut self, work: &Work, attempt: Attempt, timestamp: Option<OffsetDateTime>) {
        if self
            .snapshot
            .best
            .as_ref()
            .is_none_or(|best| attempt.hash < best.hash)
        {
            self.snapshot.best = Some(Best {
                hash: attempt.hash,
                header: attempt.header,
                job: work.job.id.clone(),
                nonce2: hex::encode(&work.extranonce2),
                recorded_at: Some(timestamp.unwrap_or_else(OffsetDateTime::now_utc)),
            });
            self.best_dirty = true;
        }
    }

    fn submit(
        &mut self,
        work: &Work,
        nonce: u32,
        hash: Hash256,
        socket: &mut Stratum,
        shares: &mut PendingShares,
    ) -> Result<()> {
        let is_block = hash <= work.job.network_target;
        if work.paid_sats > 0 && self.submitted.insert((work.extranonce2.clone(), nonce)) {
            let hash = hex::encode(hash);
            if is_block {
                self.snapshot.network_hits += 1;
                self.snapshot.candidate = Some(format!("Network target met: {hash}"));
                self.save_best();
                self.publish();
            }
            let id = socket.call(
                "mining.submit",
                json!([
                    self.config.user,
                    work.job.id,
                    hex::encode(&work.extranonce2),
                    format!("{:08x}", work.job.time),
                    format!("{nonce:08x}")
                ]),
            )?;
            shares.insert(id, (is_block, hash.clone()));
            if is_block {
                self.snapshot.candidate = Some(format!(
                    "Block candidate submitted, waiting for the pool: {hash}"
                ));
            }
        }
        Ok(())
    }
}

fn parse_extranonces(prefix: &Value, size: &Value) -> Result<(Vec<u8>, usize)> {
    let prefix = hex::decode(prefix.as_str().context("Missing extranonce1")?)?;
    let size = size.as_u64().context("Missing extranonce2 size")?;
    ensure!(prefix.len() <= 32 && size <= 32, "Extranonce is too long");
    Ok((prefix, size as usize))
}

fn read_txids(work: &Work, result: &Value, error: &Value) -> Result<Vec<Txid>> {
    ensure!(error.is_null(), "{error}");
    let txids: Vec<Txid> = if let Some(values) = result.as_str() {
        values
            .split_ascii_whitespace()
            .map(Txid::from_str)
            .collect::<Result<_, _>>()?
    } else if let Some(values) = result.as_array() {
        values
            .iter()
            .map(|item| Ok(Txid::from_str(item.as_str().context("Invalid TXID")?)?))
            .collect::<Result<_>>()?
    } else {
        bail!("the pool did not return a TXID list");
    };
    work.verify_txids(&txids)?;
    Ok(txids)
}
