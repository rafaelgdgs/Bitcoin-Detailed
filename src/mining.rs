use anyhow::{Context, Result, bail, ensure};
use bitcoin::consensus::encode::{deserialize, serialize};
use bitcoin::hashes::Hash;
use bitcoin::{CompactTarget, Script, ScriptBuf, Target, Transaction, Txid};
use num_bigint::BigUint;
use serde_json::Value;
use sha2::{Digest, Sha256};

pub type Hash256 = [u8; 32];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub id: String,
    pub prev_stratum: Hash256,
    pub coinbase_prefix: Vec<u8>,
    pub coinbase_suffix: Vec<u8>,
    pub branches: Vec<Hash256>,
    pub version: u32,
    pub bits: u32,
    pub time: u32,
    pub clean: bool,
    pub network_target: Hash256,
    pub share_target: Option<Hash256>,
    pub share_difficulty: Option<String>,
}

impl Job {
    pub fn submission_target(&self) -> Hash256 {
        self.share_target
            .map_or(self.network_target, |share| share.max(self.network_target))
    }

    pub fn parse(params: &Value, share: Option<&(String, Hash256)>) -> Result<Self> {
        let field = |n: usize| {
            params[n]
                .as_str()
                .with_context(|| format!("mining.notify: missing field {n}"))
        };
        let bits = hex_u32(field(6)?)?;
        let network_target = Target::from_compact(CompactTarget::from_consensus(bits));
        // Reject values beyond uint256 before expanding, as well as impossible mainnet targets.
        let exponent = bits >> 24;
        let mantissa = bits & 0x007f_ffff;
        ensure!(
            exponent <= 34
                && (exponent <= 33 || mantissa <= 0xff)
                && (exponent <= 32 || mantissa <= 0xffff)
                && bits & 0x0080_0000 == 0
                && network_target > Target::ZERO
                && network_target <= Target::MAX_ATTAINABLE_MAINNET,
            "Invalid mainnet nBits: {bits:08x}"
        );
        let branches = params[4].as_array().context("Missing Merkle branch")?;
        ensure!(branches.len() <= 32, "Merkle branch is too long");
        let branches = branches
            .iter()
            .map(|branch| hex_array(branch.as_str().context("Invalid Merkle node")?))
            .collect::<Result<_>>()?;
        Ok(Self {
            id: field(0)?.to_owned(),
            prev_stratum: hex_array(field(1)?)?,
            coinbase_prefix: hex::decode(field(2)?)?,
            coinbase_suffix: hex::decode(field(3)?)?,
            branches,
            version: hex_u32(field(5)?)?,
            bits,
            time: hex_u32(field(7)?)?,
            clean: params[8].as_bool().context("Missing clean_jobs flag")?,
            network_target: network_target.to_be_bytes(),
            share_target: share.map(|(_, target)| *target),
            share_difficulty: share.map(|(difficulty, _)| difficulty.clone()),
        })
    }
}

pub struct Work {
    pub job: Job,
    pub extranonce1: Vec<u8>,
    pub extranonce2: Vec<u8>,
    pub coinbase_bytes: Vec<u8>,
    pub coinbase: Transaction,
    pub paid_sats: u64,
    pub payout_script: ScriptBuf,
    pub path: Vec<Hash256>,
    header: [u8; 80],
    midstate: Sha256,
}

impl Work {
    pub fn new(
        job: Job,
        extranonce1: &[u8],
        extranonce2: Vec<u8>,
        payout: &Script,
    ) -> Result<Self> {
        let coinbase_bytes = [
            &job.coinbase_prefix[..],
            extranonce1,
            &extranonce2,
            &job.coinbase_suffix[..],
        ]
        .concat();
        let coinbase: Transaction =
            deserialize(&coinbase_bytes).context("Invalid coinbase transaction")?;
        ensure!(
            coinbase.is_coinbase(),
            "The job does not contain a coinbase transaction"
        );
        let paid_sats = coinbase
            .output
            .iter()
            .filter(|output| output.script_pubkey.as_script() == payout)
            .try_fold(0u64, |sum, output| sum.checked_add(output.value.to_sat()))
            .context("Invalid coinbase payout amount")?;

        let mut root = coinbase.compute_txid().to_byte_array();
        let mut path = vec![root];
        for branch in &job.branches {
            root = merkle_pair(&root, branch);
            path.push(root);
        }
        let mut header = [0; 80];
        header[..4].copy_from_slice(&job.version.to_le_bytes());
        // Stratum V1 uses 32-bit words in the opposite byte order to the wire header.
        for (dest, source) in header[4..36]
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(job.prev_stratum.as_chunks::<4>().0)
        {
            dest.copy_from_slice(source);
            dest.reverse();
        }
        header[36..68].copy_from_slice(&root);
        header[68..72].copy_from_slice(&job.time.to_le_bytes());
        header[72..76].copy_from_slice(&job.bits.to_le_bytes());

        // The nonce lives in the second SHA-256 block. Cache the first block's state.
        let mut midstate = Sha256::new();
        midstate.update(&header[..64]);

        Ok(Self {
            job,
            extranonce1: extranonce1.to_vec(),
            extranonce2,
            coinbase_bytes,
            coinbase,
            paid_sats,
            payout_script: payout.to_owned(),
            path,
            header,
            midstate,
        })
    }

    pub fn verify_txids(&self, txids: &[Txid]) -> Result<()> {
        let mut level = Vec::with_capacity(txids.len() + 1);
        level.push(self.coinbase.compute_txid().to_byte_array());
        level.extend(txids.iter().map(|id| id.to_byte_array()));
        for sibling in &self.job.branches {
            ensure!(
                level.get(1) == Some(sibling),
                "The TXID list does not match the job’s Merkle branch"
            );
            let mut next = Vec::with_capacity(level.len().div_ceil(2));
            for pair in level.chunks(2) {
                ensure!(
                    pair.len() == 1 || pair[0] != pair[1],
                    "The Merkle tree contains a duplicate pair"
                );
                next.push(merkle_pair(&pair[0], pair.last().unwrap()));
            }
            level = next;
        }
        ensure!(
            level.len() == 1 && level[0] == self.path[self.path.len() - 1],
            "The TXID list does not reproduce the Merkle root"
        );
        Ok(())
    }

    pub fn try_nonce(&self, nonce: u32) -> Attempt {
        self.attempt(nonce, self.hash_nonce(nonce))
    }

    pub fn header(&self) -> [u8; 80] {
        self.header
    }

    #[inline]
    pub fn hash_nonce(&self, nonce: u32) -> NonceHash {
        let mut tail: [u8; 16] = self.header[64..].try_into().unwrap();
        tail[12..].copy_from_slice(&nonce.to_le_bytes());
        let first = self.midstate.clone().chain_update(tail).finalize().into();
        let mut hash: Hash256 = Sha256::digest(first).into();
        hash.reverse();
        NonceHash { first, hash }
    }

    pub fn attempt(&self, nonce: u32, result: NonceHash) -> Attempt {
        let mut header = self.header;
        header[76..80].copy_from_slice(&nonce.to_le_bytes());
        Attempt {
            header,
            first: result.first,
            hash: result.hash,
        }
    }

    pub fn payout_offset(&self) -> usize {
        let witness_len: usize = if self
            .coinbase
            .input
            .iter()
            .any(|input| !input.witness.is_empty())
        {
            self.coinbase
                .input
                .iter()
                .map(|input| serialize(&input.witness).len())
                .sum()
        } else {
            0
        };
        let outputs_len: usize = self
            .coinbase
            .output
            .iter()
            .map(|output| serialize(output).len())
            .sum();
        self.coinbase_bytes.len() - 4 - witness_len - outputs_len
    }
}

#[derive(Clone, Copy)]
pub struct NonceHash {
    first: Hash256,
    pub hash: Hash256,
}

#[derive(Clone, Copy, Debug)]
pub struct Attempt {
    pub header: [u8; 80],
    pub first: Hash256,
    // Big-endian so bytewise comparisons match numeric Bitcoin proof of work.
    pub hash: Hash256,
}

impl Attempt {
    pub fn nonce(&self) -> u32 {
        u32::from_le_bytes(self.header[76..80].try_into().unwrap())
    }
}

pub fn hash256(bytes: &[u8]) -> Hash256 {
    Sha256::digest(Sha256::digest(bytes)).into()
}

fn merkle_pair(left: &Hash256, right: &Hash256) -> Hash256 {
    let mut pair = [0; 64];
    pair[..32].copy_from_slice(left);
    pair[32..].copy_from_slice(right);
    hash256(&pair)
}

pub fn hex_array<const N: usize>(input: &str) -> Result<[u8; N]> {
    let mut bytes = [0; N];
    hex::decode_to_slice(input, &mut bytes)?;
    Ok(bytes)
}

fn hex_u32(input: &str) -> Result<u32> {
    Ok(u32::from_be_bytes(hex_array(input)?))
}

pub fn parse_nonce(input: &str) -> Result<u32> {
    let input = input.trim();
    Ok(
        if let Some(hex) = input
            .strip_prefix("0x")
            .or_else(|| input.strip_prefix("0X"))
        {
            u32::from_str_radix(hex, 16)?
        } else {
            input.parse()?
        },
    )
}

pub fn increment(bytes: &mut [u8]) -> bool {
    for byte in bytes.iter_mut().rev() {
        let (next, carry) = byte.overflowing_add(1);
        *byte = next;
        if !carry {
            return true;
        }
    }
    false
}

pub fn hash_difficulty(hash: &Hash256) -> f64 {
    Target::from_be_bytes(*hash).difficulty_float()
}

pub fn leading_zero_bits(hash: &Hash256) -> u16 {
    let mut count = 0;
    for byte in hash {
        count += byte.leading_zeros() as u16;
        if *byte != 0 {
            break;
        }
    }
    count
}

pub fn share_target(value: &Value) -> Result<(String, Hash256)> {
    let source = match value {
        Value::Number(number) => number.to_string(),
        Value::String(s) => s.clone(),
        _ => bail!("Missing share difficulty"),
    };
    let (digits, exp) = match source.split_once(['e', 'E']) {
        Some((digits, exponent)) => (digits, exponent.parse::<i32>()?),
        None => (source.as_str(), 0),
    };
    let (whole, fractional) = digits.split_once('.').unwrap_or((digits, ""));
    ensure!(
        digits.len() <= 100 && (-100..=100).contains(&exp),
        "Share difficulty is out of range"
    );
    let mantissa: BigUint = format!("{whole}{fractional}").parse()?;
    ensure!(
        mantissa != BigUint::ZERO,
        "Share difficulty must be positive"
    );
    // Stratum's difficulty 1 is Bitcoin's 0x1d00ffff target.
    let base = BigUint::from_bytes_be(&Target::MAX_ATTAINABLE_MAINNET.to_be_bytes());
    let scale = exp - fractional.len() as i32;
    let power = BigUint::from(10u8).pow(scale.unsigned_abs());
    let target = if scale < 0 {
        base * power / mantissa
    } else {
        base / (mantissa * power)
    };
    let bytes = target.to_bytes_be();
    let mut result = [0; 32];
    if bytes.len() > 32 {
        result.fill(255);
    } else {
        result[32 - bytes.len()..].copy_from_slice(&bytes);
    }
    Ok((source, result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{Block, Network, blockdata::constants::genesis_block, merkle_tree};
    use serde_json::json;

    #[test]
    fn reconstructs_historical_mainnet_block_one() -> Result<()> {
        // Mainnet block 1, with a nonzero previous hash and the published nonce.
        let raw = hex::decode(concat!(
            "010000006fe28c0ab6f1b372c1a6a246ae63f74f931e8365e15a089c68d6190000000000",
            "982051fd1e4ba744bbbe680e1fee14677ba1a3c3540bf7b1cdb606e857233e0e",
            "61bc6649ffff001d01e3629901",
            "01000000010000000000000000000000000000000000000000000000000000000000000000",
            "ffffffff0704ffff001d0104ffffffff0100f2052a0100000043",
            "410496b538e853519c726a2c91e61ec11600ae1390813a627c66fb8be7947be63c52da7589",
            "379515d4e0a604f8141781e62294721166bf621e73a82cbf2342c858eeac00000000",
        ))?;
        let block: Block = deserialize(&raw)?;
        let coinbase = &raw[81..];
        let job = Job::parse(
            &json!([
                "block-one",
                "0a8ce26f72b3f1b646a2a6c14ff763ae65831e939c085ae10019d66800000000",
                hex::encode(&coinbase[..46]),
                hex::encode(&coinbase[48..]),
                [],
                "00000001",
                "1d00ffff",
                "4966bc61",
                true
            ]),
            None,
        )?;
        let script = &block.txdata[0].output[0].script_pubkey;
        let work = Work::new(job, &coinbase[46..47], coinbase[47..48].to_vec(), script)?;
        let attempt = work.try_nonce(2_573_394_689);
        assert_eq!(attempt.header, raw[..80]);
        assert_eq!(hex::encode(attempt.hash), block.block_hash().to_string());
        assert_eq!(
            block.header.prev_blockhash,
            genesis_block(Network::Bitcoin).block_hash()
        );
        assert!(attempt.hash <= work.job.network_target);
        assert_eq!(work.paid_sats, 5_000_000_000);
        assert_eq!(
            &work.coinbase_bytes[work.payout_offset() + 9..][..script.len()],
            script.as_bytes()
        );
        let next = Work::new(
            work.job.clone(),
            &work.extranonce1,
            vec![work.extranonce2[0].wrapping_add(1)],
            script,
        )?;
        assert_ne!(&next.header[..64], &work.header[..64]);
        for work in [work, next] {
            for nonce in [0, 2_573_394_689, u32::MAX] {
                let attempt = work.try_nonce(nonce);
                assert_eq!(
                    attempt.first,
                    <[u8; 32]>::from(Sha256::digest(attempt.header))
                );
                let mut expected = hash256(&attempt.header);
                expected.reverse();
                assert_eq!(attempt.hash, expected);
            }
        }
        Ok(())
    }

    #[test]
    fn verifies_ordered_txids_and_odd_merkle_level() -> Result<()> {
        let block = genesis_block(Network::Bitcoin);
        let coinbase = &block.txdata[0];
        let txids: Vec<Txid> = [1, 2].map(|n| Txid::from_byte_array([n; 32])).to_vec();
        let branches = [txids[0].to_byte_array(), merkle_pair(&[2; 32], &[2; 32])];
        let job = Job::parse(
            &json!([
                "merkle",
                "00".repeat(32),
                hex::encode(serialize(coinbase)),
                "",
                branches.map(hex::encode),
                "00000001",
                "1d00ffff",
                "495fab29",
                true
            ]),
            None,
        )?;
        let work = Work::new(job, &[], vec![], &coinbase.output[0].script_pubkey)?;
        let root = merkle_tree::calculate_root(
            std::iter::once(coinbase.compute_txid()).chain(txids.iter().copied()),
        )
        .unwrap();
        assert_eq!(work.header[36..68], root.to_byte_array());
        assert!(work.verify_txids(&txids).is_ok());
        assert!(work.verify_txids(&[txids[1], txids[0]]).is_err());
        assert!(work.verify_txids(&[txids[0], txids[1], txids[1]]).is_err());
        Ok(())
    }

    #[test]
    fn share_target_keeps_exact_fractional_difficulty() -> Result<()> {
        let (_, target) = share_target(&json!(1))?;
        assert_eq!(target, Target::MAX_ATTAINABLE_MAINNET.to_be_bytes());
        let (_, half) = share_target(&json!("2e0"))?;
        assert_eq!(
            BigUint::from_bytes_be(&half) * 2u8,
            BigUint::from_bytes_be(&target)
        );
        let (_, double) = share_target(&json!(0.5))?;
        assert_eq!(
            BigUint::from_bytes_be(&double),
            BigUint::from_bytes_be(&target) * 2u8
        );
        assert!(share_target(&json!(0)).is_err());
        Ok(())
    }
}
