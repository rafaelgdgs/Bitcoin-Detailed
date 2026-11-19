use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use bitcoin::address::NetworkUnchecked;
use bitcoin::{Address, Network, Txid};
use serde_json::{Value, json};
use std::str::FromStr;

use crate::mining::{self, Attempt, Hash256, Job, Work};
use crate::stratum::Stratum;

pub const DEFAULT_POOL: &str = "stratum.ckpool.org:3333";

pub struct Config {
    pub endpoint: String,
    pub address: Address,
    pub user: String,
    pub password: String,
}

impl Config {
    pub fn new(endpoint: &str, address: &str, worker: &str, password: &str) -> Result<Self> {
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
        Ok(Self {
            endpoint: endpoint.to_owned(),
            address,
            user,
            password: password.to_owned(),
        })
    }
}

pub enum Command {
    Toggle,
    Manual(Arc<Work>, u32),
    Txids(Arc<Work>),
    Stop,
}

#[derive(Clone)]
pub struct Best {
    pub hash: Hash256,
    pub header: [u8; 80],
    pub job: String,
    pub nonce2: String,
}

impl Best {
    fn read(path: &std::path::Path) -> Option<Self> {
        let record: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        let hash: Hash256 = mining::hex_array(record["hash"].as_str()?).ok()?;
        let header = mining::hex_array(record["header"].as_str()?).ok()?;
        let mut computed = mining::hash256(&header);
        computed.reverse();
        (hash == computed).then(|| Self {
            hash,
            header,
            job: record["job"].as_str().unwrap_or_default().to_owned(),
            nonce2: record["extranonce2"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        })
    }

    fn save(&self, path: &std::path::Path) -> Result<()> {
        let temp = path.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(
            &temp,
            serde_json::to_vec_pretty(&json!({
                "hash": hex::encode(self.hash),
                "header": hex::encode(self.header),
                "job": self.job,
                "extranonce2": self.nonce2,
            }))?,
        )?;
        std::fs::rename(temp, path)?;
        Ok(())
    }
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
    pub accepted: u64,
    pub rejected: u64,
    pub network_hits: u64,
    pub candidate: Option<String>,
    pub txids: Option<Arc<Vec<Txid>>>,
    pub tx_status: String,
}

impl Snapshot {
    fn new(best: Option<Best>) -> Self {
        Self {
            status: "Connecting…".into(),
            message: "Space starts or pauses automatic mining. N tests a nonce.".into(),
            work: None,
            last: None,
            best,
            total: 0,
            rate: 0,
            running: false,
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

pub fn start(config: Config, best_path: PathBuf) -> Connection {
    let (sender, commands) = mpsc::channel();
    let snapshot = Snapshot::new(Best::read(&best_path));
    let shared = Arc::new(Mutex::new(snapshot.clone()));
    let published = Arc::clone(&shared);
    let thread = std::thread::spawn(move || {
        let mut engine = Engine {
            config,
            commands,
            published,
            snapshot,
            best_path,
            best_dirty: false,
            automatic: false,
            next_nonce: Some(0),
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
    best_path: PathBuf,
    best_dirty: bool,
    automatic: bool,
    next_nonce: Option<u32>,
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
            self.best_dirty = false;
            if let Err(error) = best.save(&self.best_path) {
                self.snapshot.message = format!("Could not save the best hash: {error}");
            }
        }
    }

    fn run(&mut self) {
        loop {
            if let Err(error) = self.session() {
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
            loop {
                match self
                    .commands
                    .recv_timeout(until.saturating_duration_since(Instant::now()))
                {
                    Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    Ok(Command::Toggle) => self.automatic = !self.automatic,
                    Ok(_) => {}
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                }
            }
        }
    }

    fn session(&mut self) -> Result<()> {
        let mut socket = Stratum::connect(&self.config.endpoint)?;
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
            let messages = socket.read()?;
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
                            latest_job = Some(job.clone());
                            tx_request = None;
                            self.invalidate_work();
                            if authorized {
                                self.install_job(job, &extranonce1, extranonce2_size, &payout)?;
                            }
                        }
                    }
                    Some("mining.set_extranonce") => {
                        (extranonce1, extranonce2_size) =
                            parse_extranonces(&params[0], &params[1])?;
                        tx_request = None;
                        self.invalidate_work();
                        if authorized && let Some(job) = latest_job.clone() {
                            self.install_job(job, &extranonce1, extranonce2_size, &payout)?;
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
                            self.install_job(job, &extranonce1, extranonce2_size, &payout)?;
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
                    Ok(Command::Stop) | Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                    Ok(Command::Toggle) => {
                        self.automatic = !self.automatic;
                        self.snapshot.message = if self.automatic {
                            "Automatic mining started.".into()
                        } else {
                            "Mining paused.".into()
                        };
                    }
                    Ok(Command::Manual(shown, nonce)) => {
                        self.automatic = false;
                        self.snapshot.message = "Manual attempt · mining paused.".into();
                        if self
                            .snapshot
                            .work
                            .as_ref()
                            .is_some_and(|work| Arc::ptr_eq(work, &shown))
                        {
                            self.attempt(&shown, nonce, &mut socket, &mut shares)?;
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

            self.snapshot.running = self.automatic
                && self
                    .snapshot
                    .work
                    .as_ref()
                    .is_some_and(|work| work.paid_sats > 0);
            if self.snapshot.running
                && let Some(mut work) = self.snapshot.work.clone()
            {
                for _ in 0..16_384 {
                    if self.next_nonce.is_none() {
                        let mut nonce2 = work.extranonce2.clone();
                        if !mining::increment(&mut nonce2) {
                            self.automatic = false;
                            self.snapshot.running = false;
                            self.snapshot.message =
                                "Nonce space exhausted. Waiting for a new job.".into();
                            break;
                        }
                        work =
                            Arc::new(Work::new(work.job.clone(), &extranonce1, nonce2, &payout)?);
                        self.snapshot.work = Some(work.clone());
                        self.next_nonce = Some(0);
                    }
                    let nonce = self.next_nonce.unwrap();
                    self.attempt(&work, nonce, &mut socket, &mut shares)?;
                    self.next_nonce = nonce.checked_add(1);
                }
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
        self.next_nonce = Some(0);
    }

    fn install_job(
        &mut self,
        job: Job,
        extranonce1: &[u8],
        len: usize,
        payout: &bitcoin::Script,
    ) -> Result<()> {
        let work = Work::new(job, extranonce1, vec![0; len], payout)?;
        self.snapshot.status = if work.paid_sats == 0 {
            "Job received · no payout output for your address".into()
        } else {
            "Mainnet job received · payout found in the coinbase".into()
        };
        self.snapshot.work = Some(Arc::new(work));
        Ok(())
    }

    fn attempt(
        &mut self,
        work: &Work,
        nonce: u32,
        socket: &mut Stratum,
        shares: &mut HashMap<u64, (bool, String)>,
    ) -> Result<()> {
        let attempt = work.try_nonce(nonce);
        self.snapshot.last = Some(attempt);
        self.snapshot.total += 1;
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
            });
            self.best_dirty = true;
        }
        let is_block = attempt.hash <= work.job.network_target;
        let is_share = work
            .job
            .share_target
            .is_some_and(|target| attempt.hash <= target);
        if work.paid_sats > 0
            && (is_block || is_share)
            && self.submitted.insert((work.extranonce2.clone(), nonce))
        {
            let hash = hex::encode(attempt.hash);
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
