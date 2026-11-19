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

pub const BATCH_SIZE: u64 = 65_536;
const WORKGROUP_SIZE: usize = 128;
const SHARE_WORDS: usize = WORKGROUP_SIZE / 32;
const HASH_WORDS: usize = 8;
const GROUP_WORDS: usize = HASH_WORDS + 1 + SHARE_WORDS;
const INPUT_WORDS: usize = 21;
const SHADER: &str = include_str!("sha256d.wgsl");
const OUTPUT_SIZE: u64 = output_size(BATCH_SIZE as usize / WORKGROUP_SIZE);
const SHA256_IV: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

const fn output_size(groups: usize) -> u64 {
    ((HASH_WORDS + groups * GROUP_WORDS) * size_of::<u32>()) as u64
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

pub struct Miner {
    device: Device,
    queue: Queue,
    pipeline: ComputePipeline,
    bindings: wgpu::BindGroup,
    input: Buffer,
    output: Buffer,
    readback: Buffer,
    errors: mpsc::Receiver<String>,
}

impl Miner {
    pub fn new(adapter: &Adapter) -> Result<Self> {
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
            compilation_options: Default::default(),
            cache: None,
        });
        check_errors(&errors)?;
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
            OUTPUT_SIZE,
            BufferUsages::STORAGE | BufferUsages::COPY_SRC,
        );
        let readback = buffer(
            "Mining readback",
            OUTPUT_SIZE,
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
        check_errors(&errors)?;

        Ok(Self {
            device,
            queue,
            pipeline,
            bindings,
            input,
            output,
            readback,
            errors,
        })
    }

    pub fn scan(
        &self,
        work: &Work,
        range: Range<u64>,
        best_limit: Option<Hash256>,
    ) -> Result<Batch> {
        let count = (range.end - range.start) as usize;
        let groups = count.div_ceil(WORKGROUP_SIZE);
        let target = work.job.submission_target();
        let header = work.header();
        let mut midstate = SHA256_IV;
        let block: [u8; 64] = header[..64].try_into().unwrap();
        compress256(&mut midstate, &[block.into()]);
        let mut input = [0; INPUT_WORDS];
        input[..8].copy_from_slice(&midstate);
        input[8..11].copy_from_slice(&be_words::<3>(&header[64..76]));
        input[11] = range.start as u32;
        input[12] = count as u32;
        input[13..].copy_from_slice(&be_words::<8>(&target));

        check_errors(&self.errors)?;
        self.queue
            .write_buffer(&self.input, 0, bytemuck::cast_slice(&input));
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bindings, &[]);
            pass.dispatch_workgroups(groups as u32, 1, 1);
        }
        let size = output_size(groups);
        encoder.copy_buffer_to_buffer(&self.output, 0, &self.readback, 0, size);
        let submission = self.queue.submit([encoder.finish()]);
        let slice = self.readback.slice(..size);
        let (sender, receiver) = mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device.poll(wgpu::PollType::Wait {
            submission_index: Some(submission),
            timeout: Some(Duration::from_secs(5)),
        })?;
        check_errors(&self.errors)?;
        receiver
            .try_recv()
            .context("Missing Vulkan readback status")??;
        let finished = Instant::now();
        let timestamp = OffsetDateTime::now_utc();
        let data = slice.get_mapped_range()?;
        let result = read_batch(
            bytemuck::cast_slice(&data),
            work,
            range,
            target,
            best_limit,
            finished,
            timestamp,
        );
        drop(data);
        self.readback.unmap();
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

fn read_batch(
    data: &[u32],
    work: &Work,
    range: Range<u64>,
    target: Hash256,
    best_limit: Option<Hash256>,
    finished: Instant,
    timestamp: OffsetDateTime,
) -> Result<Batch> {
    let last = work.try_nonce((range.end - 1) as u32);
    ensure!(
        data[..HASH_WORDS] == be_words::<8>(&last.hash),
        "GPU and CPU disagree on the last hash"
    );

    let mut solutions = Vec::new();
    let mut minimum = None;
    let (groups, _) = data[HASH_WORDS..].as_chunks::<GROUP_WORDS>();
    for (index, group) in groups.iter().enumerate() {
        let (hash, rest) = group.split_at(HASH_WORDS);
        let (&nonce, flags) = rest.split_first().unwrap();
        let group_start = range.start + (index * WORKGROUP_SIZE) as u64;
        let group_end = (group_start + WORKGROUP_SIZE as u64).min(range.end);
        ensure!(
            (group_start..group_end).contains(&u64::from(nonce)),
            "GPU returned a minimum outside its nonce range"
        );
        if minimum.is_none_or(|(best, _)| hash < best) {
            minimum = Some((hash, nonce));
        }
        for (word, mut bits) in flags.iter().copied().enumerate() {
            while bits != 0 {
                let nonce = group_start + (word * 32 + bits.trailing_zeros() as usize) as u64;
                ensure!(
                    nonce < group_end,
                    "GPU returned a share outside its nonce range"
                );
                let nonce = nonce as u32;
                let hash = work.hash_nonce(nonce).hash;
                ensure!(hash <= target, "GPU returned a share above the target");
                solutions.push((nonce, hash));
                bits &= bits - 1;
            }
        }
    }
    let (hash, nonce) = minimum.context("GPU did not return a batch minimum")?;
    let attempt = work.try_nonce(nonce);
    ensure!(
        hash == be_words::<8>(&attempt.hash),
        "GPU and CPU disagree on the best hash"
    );
    let best = best_limit
        .is_none_or(|limit| attempt.hash < limit)
        .then_some((attempt, timestamp));
    Ok(Batch {
        count: range.end - range.start,
        last,
        best,
        solutions,
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

    #[test]
    #[ignore = "requires access to a hardware Vulkan adapter"]
    fn vulkan_matches_cpu_for_block_candidate_best_hash_and_shares() -> Result<()> {
        let adapters = adapters();
        ensure!(
            !adapters.is_empty(),
            "No hardware Vulkan adapter is available"
        );
        let genesis = genesis_block(Network::Bitcoin);
        let coinbase = &genesis.txdata[0];
        let job = Job::parse(
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
        let mut work = Work::new(job, &[], vec![], &coinbase.output[0].script_pubkey)?;
        let nonce = u64::from(genesis.header.nonce);
        let known = work.try_nonce(nonce as u32);
        assert_eq!(hex::encode(known.hash), genesis.block_hash().to_string());
        for adapter in adapters {
            println!("Testing {}", adapter.get_info().name);
            let miner = Miner::new(&adapter)?;
            // The network target alone must suffice for a real, historical block candidate.
            work.job.share_target = None;
            let range = nonce - 63..nonce + 67;
            let batch = miner.scan(&work, range.clone(), None)?;
            assert_eq!(batch.solutions, vec![(nonce as u32, known.hash)]);
            assert_eq!(batch.best.unwrap().0.hash, known.hash);
            assert_eq!(batch.last.nonce(), (range.end - 1) as u32);

            // Test all 32-bit nonce edge cases, including a partial final workgroup and equality.
            let range = NONCE_SPACE - 393..NONCE_SPACE;
            let last = work.try_nonce(u32::MAX);
            for target in [last.hash, [255; 32]] {
                work.job.share_target = Some(target);
                let expected = range
                    .clone()
                    .map(|nonce| (nonce as u32, work.hash_nonce(nonce as u32).hash))
                    .collect::<Vec<_>>();
                let batch = miner.scan(&work, range.clone(), None)?;
                assert_eq!(batch.count, range.end - range.start);
                assert_eq!(batch.last.hash, last.hash);
                assert_eq!(
                    batch.best.unwrap().0.hash,
                    expected.iter().map(|(_, hash)| *hash).min().unwrap()
                );
                assert_eq!(
                    batch.solutions,
                    expected
                        .into_iter()
                        .filter(|(_, hash)| *hash <= work.job.submission_target())
                        .collect::<Vec<_>>()
                );
            }
            assert!(miner.scan(&work, range, Some([0; 32]))?.best.is_none());
        }
        Ok(())
    }
}
