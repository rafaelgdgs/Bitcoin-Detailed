use std::ops::Range;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use futures_lite::future::block_on;
use sha2::compress256;
use time::OffsetDateTime;
use wgpu::{Adapter, Backends, Buffer, BufferUsages, ComputePipeline, Device, Instance, Queue};

use crate::mining::{Hash256, Work};
use crate::workers::Batch;

pub const SLOT_COUNT: usize = 2;
const BATCH_SIZE: u64 = 1_048_576;
const WORKGROUP_SIZE: u32 = 128;
const HASH_WORDS: usize = 8;
const HASH_BYTES: u64 = (HASH_WORDS * size_of::<u32>()) as u64;
const INPUT_WORDS: usize = 29;
const SHADER: &str = include_str!("sha256d.wgsl");
const SHA256_IV: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];
const INITIAL_ROUND_CONSTANTS: [u32; 3] = [0x428a2f98, 0x71374491, 0xb5c0fbcf];
// About one observation per full batch. The existing bitmap has room for every match.
const OBSERVATION_TARGET: Hash256 = {
    let mut target = [u8::MAX; 32];
    target[0] = 0;
    target[1] = 0;
    target[2] = 0x0f;
    target
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Intensity(u8);

impl Intensity {
    pub const FULL: Self = Self(100);

    pub fn new(percent: u8) -> Option<Self> {
        (1..=Self::FULL.0)
            .contains(&percent)
            .then_some(Self(percent))
    }

    pub fn percent(self) -> u8 {
        self.0
    }

    pub fn idle_after(self, active: Duration) -> Duration {
        active * u32::from(Self::FULL.0 - self.0) / u32::from(self.0)
    }
}

impl Default for Intensity {
    fn default() -> Self {
        Self::FULL
    }
}

const fn output_size(nonces: u64) -> u64 {
    HASH_BYTES + nonces.div_ceil(32) * size_of::<u32>() as u64
}

pub fn batch_size(work: &Work, best_limit: Option<Hash256>) -> u64 {
    let Some(best) = best_limit else {
        // Mark all nonces in a short first batch to establish an exact initial record.
        return 256;
    };
    let threshold = candidate_threshold(work.job.submission_target(), Some(best));
    let high = u64::from(be_words::<1>(&threshold)[0]) + 1;
    // Aim for at most 4,096 candidates when a pool sets an unusually easy target.
    // The bitset still has space for every nonce that actually qualifies.
    BATCH_SIZE.min((4_096 << 32) / high)
}

fn candidate_threshold(share_target: Hash256, best_limit: Option<Hash256>) -> Hash256 {
    best_limit.map_or([u8::MAX; 32], |best| {
        best.max(share_target).max(OBSERVATION_TARGET)
    })
}

pub fn adapters() -> Vec<Adapter> {
    if !Instance::enabled_backend_features().contains(Backends::VULKAN) {
        return Vec::new();
    }
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = Backends::VULKAN;
    let instance = Instance::new(descriptor);
    block_on(instance.enumerate_adapters(Backends::VULKAN))
        .into_iter()
        .filter(|adapter| {
            adapter.get_info().device_type != wgpu::DeviceType::Cpu
                && adapter
                    .get_downlevel_capabilities()
                    .flags
                    .contains(wgpu::DownlevelFlags::COMPUTE_SHADERS)
        })
        .collect()
}

struct Slot {
    bindings: wgpu::BindGroup,
    input: Buffer,
    output: Buffer,
    readback: Buffer,
}

pub struct Dispatch {
    slot: usize,
    work: Arc<Work>,
    range: Range<u64>,
    best_limit: Option<Hash256>,
    submission: wgpu::SubmissionIndex,
    mapped: mpsc::Receiver<Result<(Instant, OffsetDateTime), wgpu::BufferAsyncError>>,
}

impl Dispatch {
    pub fn start(&self) -> u64 {
        self.range.start
    }
}

pub struct Miner {
    device: Device,
    queue: Queue,
    pipeline: ComputePipeline,
    slots: [Slot; SLOT_COUNT],
    workgroup_size: u32,
    cached_input: Option<(Arc<Work>, [u32; INPUT_WORDS])>,
    errors: mpsc::Receiver<String>,
}

impl Miner {
    pub fn new(adapter: &Adapter) -> Result<Self> {
        Self::with_workgroup(adapter, WORKGROUP_SIZE)
    }

    fn with_workgroup(adapter: &Adapter, workgroup_size: u32) -> Result<Self> {
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            required_limits: wgpu::Limits::downlevel_defaults(),
            ..Default::default()
        }))?;
        let (sender, errors) = mpsc::channel();
        let uncaptured = sender.clone();
        device.on_uncaptured_error(Arc::new(move |error: wgpu::Error| {
            let _ = uncaptured.send(error.to_string());
        }));
        device.set_device_lost_callback(move |reason, message| {
            let _ = sender.send(format!("Vulkan device lost ({reason:?}): {message}"));
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Bitcoin SHA-256d"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("Bitcoin nonce scan"),
            layout: None,
            module: &shader,
            entry_point: Some("scan"),
            compilation_options: wgpu::PipelineCompilationOptions {
                constants: &[("WORKGROUP_SIZE", f64::from(workgroup_size))],
                ..Default::default()
            },
            cache: None,
        });
        check_errors(&errors)?;
        let slots = std::array::from_fn(|_| Self::make_slot(&device, &pipeline));
        check_errors(&errors)?;

        Ok(Self {
            device,
            queue,
            pipeline,
            slots,
            workgroup_size,
            cached_input: None,
            errors,
        })
    }

    fn make_slot(device: &Device, pipeline: &ComputePipeline) -> Slot {
        let buffer = |label, size, usage| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        let input = buffer(
            "Mining job",
            (INPUT_WORDS * size_of::<u32>()) as u64,
            BufferUsages::STORAGE | BufferUsages::COPY_DST,
        );
        let output = buffer(
            "Mining results",
            output_size(BATCH_SIZE),
            BufferUsages::STORAGE | BufferUsages::COPY_SRC | BufferUsages::COPY_DST,
        );
        let readback = buffer(
            "Mining readback",
            output_size(BATCH_SIZE),
            BufferUsages::MAP_READ | BufferUsages::COPY_DST,
        );
        let bindings = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Mining job and results"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: input.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: output.as_entire_binding(),
                },
            ],
        });
        Slot {
            bindings,
            input,
            output,
            readback,
        }
    }

    pub fn submit(
        &mut self,
        slot: usize,
        work: Arc<Work>,
        range: Range<u64>,
        best_limit: Option<Hash256>,
    ) -> Result<Dispatch> {
        check_errors(&self.errors)?;
        let count = range.end - range.start;
        debug_assert!((1..=BATCH_SIZE).contains(&count));
        let mut input = match &self.cached_input {
            Some((previous, input)) if Arc::ptr_eq(previous, &work) => *input,
            _ => {
                let input = prepare_input(&work);
                self.cached_input = Some((work.clone(), input));
                input
            }
        };
        let threshold = candidate_threshold(work.job.submission_target(), best_limit);
        input[19] = range.start as u32;
        input[20] = count as u32;
        input[21..].copy_from_slice(&be_words::<8>(&threshold));

        let buffers = &self.slots[slot];
        self.queue
            .write_buffer(&buffers.input, 0, bytemuck::cast_slice(&input));
        let mut encoder = self.device.create_command_encoder(&Default::default());
        let size = output_size(count);
        encoder.clear_buffer(&buffers.output, HASH_BYTES, Some(size - HASH_BYTES));
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &buffers.bindings, &[]);
            pass.dispatch_workgroups((count as u32).div_ceil(self.workgroup_size), 1, 1);
        }
        encoder.copy_buffer_to_buffer(&buffers.output, 0, &buffers.readback, 0, size);
        let (sender, mapped) = mpsc::channel();
        encoder.map_buffer_on_submit(
            &buffers.readback,
            wgpu::MapMode::Read,
            ..size,
            move |result| {
                let _ = sender.send(result.map(|()| (Instant::now(), OffsetDateTime::now_utc())));
            },
        );
        let submission = self.queue.submit([encoder.finish()]);
        Ok(Dispatch {
            slot,
            work,
            range,
            best_limit,
            submission,
            mapped,
        })
    }

    pub fn collect(&self, dispatch: Dispatch) -> Result<Batch> {
        self.device.poll(wgpu::PollType::Wait {
            submission_index: Some(dispatch.submission),
            timeout: Some(Duration::from_secs(5)),
        })?;
        check_errors(&self.errors)?;
        let (finished, timestamp) = dispatch
            .mapped
            .try_recv()
            .context("Missing Vulkan readback status")??;
        let count = dispatch.range.end - dispatch.range.start;
        let readback = &self.slots[dispatch.slot].readback;
        let data = readback.slice(..output_size(count)).get_mapped_range()?;
        let result = read_batch(
            bytemuck::cast_slice(&data),
            &dispatch.work,
            dispatch.range,
            dispatch.best_limit,
            finished,
            timestamp,
        );
        drop(data);
        readback.unmap();
        result
    }
}

fn check_errors(errors: &mpsc::Receiver<String>) -> Result<()> {
    if let Ok(error) = errors.try_recv() {
        anyhow::bail!("{error}");
    }
    Ok(())
}

fn be_words<const N: usize>(bytes: &[u8]) -> [u32; N] {
    std::array::from_fn(|i| u32::from_be_bytes(bytes[i * 4..][..4].try_into().unwrap()))
}

fn prepare_input(work: &Work) -> [u32; INPUT_WORDS] {
    let header = work.header();
    let mut midstate = SHA256_IV;
    let block: [u8; 64] = header[..64].try_into().unwrap();
    compress256(&mut midstate, &[block.into()]);
    let tail = be_words::<3>(&header[64..76]);
    let mut state = midstate;
    for (word, constant) in tail.into_iter().zip(INITIAL_ROUND_CONSTANTS) {
        let [a, b, c, d, e, f, g, h] = state;
        let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let choice = (e & f) ^ (!e & g);
        let t1 = h
            .wrapping_add(sum1)
            .wrapping_add(choice)
            .wrapping_add(constant)
            .wrapping_add(word);
        let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        state = [
            t1.wrapping_add(sum0).wrapping_add(majority),
            a,
            b,
            c,
            d.wrapping_add(t1),
            e,
            f,
            g,
        ];
    }

    let mut input = [0; INPUT_WORDS];
    input[..8].copy_from_slice(&midstate);
    input[8..16].copy_from_slice(&state);
    input[16..19].copy_from_slice(&tail);
    input
}

fn read_batch(
    data: &[u32],
    work: &Work,
    range: Range<u64>,
    mut best_limit: Option<Hash256>,
    finished: Instant,
    timestamp: OffsetDateTime,
) -> Result<Batch> {
    let last = work.try_nonce((range.end - 1) as u32);
    ensure!(
        data[..HASH_WORDS] == be_words::<8>(&last.hash),
        "GPU and CPU disagree on the last hash"
    );

    let mut best = None;
    let mut sample = last.hash;
    let mut solutions = Vec::new();
    let target = work.job.submission_target();
    let threshold = candidate_threshold(target, best_limit);
    let flags = &data[HASH_WORDS..];
    let last_index = (range.end - range.start - 1) as usize;
    let last_marked = flags[last_index / 32] & (1 << (last_index % 32)) != 0;
    ensure!(
        last_marked == (last.hash <= threshold),
        "GPU and CPU disagree on whether the last hash met the threshold"
    );
    for (word, mut bits) in flags.iter().copied().enumerate() {
        while bits != 0 {
            let nonce = range.start + (word * 32 + bits.trailing_zeros() as usize) as u64;
            ensure!(
                nonce < range.end,
                "GPU returned a candidate outside its nonce range"
            );
            let nonce = nonce as u32;
            let result = work.hash_nonce(nonce);
            ensure!(
                result.hash <= threshold,
                "GPU returned a candidate above the threshold"
            );
            sample = sample.min(result.hash);
            if result.hash <= target {
                solutions.push((nonce, result.hash));
            }
            if best_limit.is_none_or(|limit| result.hash < limit) {
                best_limit = Some(result.hash);
                best = Some((work.attempt(nonce, result), timestamp));
            }
            bits &= bits - 1;
        }
    }
    Ok(Batch {
        count: range.end - range.start,
        last,
        best,
        solutions,
        sample,
        finished,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::Network;
    use bitcoin::blockdata::constants::genesis_block;
    use bitcoin::consensus::encode::serialize;
    use serde_json::json;

    use crate::mining::Job;
    use crate::workers::NONCE_SPACE;

    #[test]
    fn validates_compute_shader_without_vulkan_hardware() -> Result<()> {
        use wgpu::naga::{front::wgsl, valid};

        let module = wgsl::parse_str(SHADER)
            .map_err(|error| anyhow::anyhow!("{}", error.emit_to_string(SHADER)))?;
        valid::Validator::new(valid::ValidationFlags::all(), valid::Capabilities::empty())
            .validate(&module)
            .map_err(|error| anyhow::anyhow!("{}", error.emit_to_string(SHADER)))?;
        Ok(())
    }

    fn test_work(share_target: Option<Hash256>) -> Result<Arc<Work>> {
        let genesis = genesis_block(Network::Bitcoin);
        let coinbase = &genesis.txdata[0];
        let mut job = Job::parse(
            &json!([
                "genesis",
                "00".repeat(32),
                hex::encode(serialize(coinbase)),
                "",
                [],
                "00000001",
                "1d00ffff",
                "495fab29",
                true
            ]),
            None,
        )?;
        job.share_target = share_target;
        Ok(Arc::new(Work::new(
            job,
            &[],
            vec![],
            &coinbase.output[0].script_pubkey,
        )?))
    }

    fn assert_batch(work: &Work, range: Range<u64>, best_limit: Option<Hash256>, batch: Batch) {
        let expected = range
            .clone()
            .map(|nonce| (nonce as u32, work.hash_nonce(nonce as u32).hash))
            .collect::<Vec<_>>();
        let best = expected.iter().map(|(_, hash)| *hash).min().unwrap();
        assert_eq!(batch.count, range.end - range.start);
        assert_eq!(batch.last.nonce(), expected.last().unwrap().0);
        assert_eq!(batch.last.hash, expected.last().unwrap().1);
        assert_eq!(
            batch.best.map(|(attempt, _)| attempt.hash),
            best_limit.is_none_or(|limit| best < limit).then_some(best)
        );
        assert_eq!(
            batch.solutions,
            expected
                .into_iter()
                .filter(|(_, hash)| *hash <= work.job.submission_target())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    #[ignore = "requires access to a hardware Vulkan adapter"]
    fn vulkan_matches_cpu_for_block_candidate_best_hash_and_shares() -> Result<()> {
        let adapters = adapters();
        ensure!(
            !adapters.is_empty(),
            "No hardware Vulkan adapter is available"
        );
        let work = test_work(None)?;
        let genesis = genesis_block(Network::Bitcoin);
        let nonce = u64::from(genesis.header.nonce);
        let known = work.try_nonce(nonce as u32);
        assert_eq!(hex::encode(known.hash), genesis.block_hash().to_string());
        let genesis_range = nonce - 63..nonce + 67;
        let near_end = NONCE_SPACE - 393..NONCE_SPACE;
        // This nonce crosses the observation threshold on the historic genesis header.
        let sample_nonce = 1_085_914;
        let sample_hash = work.try_nonce(sample_nonce).hash;
        assert!(sample_hash <= OBSERVATION_TARGET && sample_hash > work.job.network_target);
        let equality = test_work(Some(work.hash_nonce(u32::MAX).hash))?;
        let any_share = test_work(Some([u8::MAX; 32]))?;

        for adapter in adapters {
            println!("Testing {}", adapter.get_info().name);
            let mut miner = Miner::new(&adapter)?;
            // Scan overlapping submissions on distinct slots, including an inclusive target and
            // a partial workgroup at the end of the nonce space.
            let first = miner.submit(0, work.clone(), genesis_range.clone(), Some([0; 32]))?;
            let second = miner.submit(1, equality.clone(), near_end.clone(), Some([0; 32]))?;
            let batch = miner.collect(first)?;
            assert_eq!(batch.solutions, vec![(nonce as u32, known.hash)]);
            assert_batch(&work, genesis_range.clone(), Some([0; 32]), batch);

            let third = miner.submit(0, any_share.clone(), near_end.clone(), None)?;
            assert_batch(
                &equality,
                near_end.clone(),
                Some([0; 32]),
                miner.collect(second)?,
            );
            assert_batch(&any_share, near_end.clone(), None, miner.collect(third)?);

            // The slot had every bit set. Reusing it must report only hashes that meet the
            // current record or share target, even if the previous result was larger.
            let cutoff = work.hash_nonce(nonce as u32 + 3).hash;
            let reused = miner.submit(0, work.clone(), genesis_range.clone(), Some(cutoff))?;
            assert_batch(
                &work,
                genesis_range.clone(),
                Some(cutoff),
                miner.collect(reused)?,
            );
            let bootstrap = miner.submit(1, work.clone(), near_end.clone(), None)?;
            assert_batch(&work, near_end.clone(), None, miner.collect(bootstrap)?);
            let range = u64::from(sample_nonce)..u64::from(sample_nonce) + 2;
            let observation = miner.submit(0, work.clone(), range, Some([0; 32]))?;
            let batch = miner.collect(observation)?;
            assert_eq!(batch.sample, sample_hash);
            assert!(batch.best.is_none() && batch.solutions.is_empty());
        }
        Ok(())
    }

    #[test]
    #[ignore = "benchmarks a hardware Vulkan adapter"]
    fn vulkan_benchmark_tuning() -> Result<()> {
        let adapters = adapters();
        ensure!(
            !adapters.is_empty(),
            "No hardware Vulkan adapter is available"
        );
        let work = test_work(None)?;
        for adapter in adapters {
            println!("Benchmarking {}", adapter.get_info().name);
            for group in [64, 128, 256] {
                let mut miner = Miner::with_workgroup(&adapter, group)?;
                for size in [65_536, 262_144, BATCH_SIZE] {
                    let depth = SLOT_COUNT;
                    let mut samples = [0.0; 3];
                    for sample in &mut samples {
                        *sample = benchmark(&mut miner, &work, size, depth)?;
                    }
                    samples.sort_by(f64::total_cmp);
                    println!(
                        "{group:>3} lanes, {size:>7} nonces, {depth} slots: {:>7.2} MH/s",
                        samples[1]
                    );
                }
                if group == WORKGROUP_SIZE {
                    let mut samples = [0.0; 3];
                    for sample in &mut samples {
                        *sample = benchmark(&mut miner, &work, BATCH_SIZE, 1)?;
                    }
                    samples.sort_by(f64::total_cmp);
                    println!(
                        "{group:>3} lanes, {BATCH_SIZE:>7} nonces, 1 slot:  {:>7.2} MH/s",
                        samples[1]
                    );
                }
            }
        }
        Ok(())
    }

    fn benchmark(miner: &mut Miner, work: &Arc<Work>, size: u64, depth: usize) -> Result<f64> {
        use std::collections::VecDeque;

        let start = Instant::now();
        let duration = Duration::from_millis(350);
        let mut next = 0;
        let mut pending = VecDeque::new();
        while start.elapsed() < duration {
            while pending.len() < depth {
                let slot = (next / size) as usize % depth;
                let start = next % NONCE_SPACE;
                pending.push_back(miner.submit(
                    slot,
                    work.clone(),
                    start..start + size,
                    Some([0; 32]),
                )?);
                next += size;
            }
            miner.collect(pending.pop_front().unwrap())?;
        }
        for dispatch in pending {
            miner.collect(dispatch)?;
        }
        Ok(next as f64 / start.elapsed().as_secs_f64() / 1e6)
    }
}
