alias Hash = array<u32, 8>;

const IV = Hash(
    0x6a09e667u, 0xbb67ae85u, 0x3c6ef372u, 0xa54ff53au,
    0x510e527fu, 0x9b05688cu, 0x1f83d9abu, 0x5be0cd19u,
);
const K = array<u32, 64>(
    0x428a2f98u, 0x71374491u, 0xb5c0fbcfu, 0xe9b5dba5u,
    0x3956c25bu, 0x59f111f1u, 0x923f82a4u, 0xab1c5ed5u,
    0xd807aa98u, 0x12835b01u, 0x243185beu, 0x550c7dc3u,
    0x72be5d74u, 0x80deb1feu, 0x9bdc06a7u, 0xc19bf174u,
    0xe49b69c1u, 0xefbe4786u, 0x0fc19dc6u, 0x240ca1ccu,
    0x2de92c6fu, 0x4a7484aau, 0x5cb0a9dcu, 0x76f988dau,
    0x983e5152u, 0xa831c66du, 0xb00327c8u, 0xbf597fc7u,
    0xc6e00bf3u, 0xd5a79147u, 0x06ca6351u, 0x14292967u,
    0x27b70a85u, 0x2e1b2138u, 0x4d2c6dfcu, 0x53380d13u,
    0x650a7354u, 0x766a0abbu, 0x81c2c92eu, 0x92722c85u,
    0xa2bfe8a1u, 0xa81a664bu, 0xc24b8b70u, 0xc76c51a3u,
    0xd192e819u, 0xd6990624u, 0xf40e3585u, 0x106aa070u,
    0x19a4c116u, 0x1e376c08u, 0x2748774cu, 0x34b0bcb5u,
    0x391c0cb3u, 0x4ed8aa4au, 0x5b9cca4fu, 0x682e6ff3u,
    0x748f82eeu, 0x78a5636fu, 0x84c87814u, 0x8cc70208u,
    0x90befffau, 0xa4506cebu, 0xbef9a3f7u, 0xc67178f2u,
);

const WORKGROUP_SIZE = 128u;
const SHARE_WORDS = WORKGROUP_SIZE / 32u;

struct Job {
    midstate: Hash,
    tail: array<u32, 3>,
    start: u32,
    count: u32,
    threshold: Hash,
}

struct Candidate {
    hash: Hash,
    nonce: u32,
}

struct Group {
    best: Candidate,
    shares: array<u32, SHARE_WORDS>,
}

struct Output {
    last: Hash,
    groups: array<Group>,
}

@group(0) @binding(0) var<storage, read> job: Job;
@group(0) @binding(1) var<storage, read_write> output: Output;
var<workgroup> candidates: array<Candidate, WORKGROUP_SIZE>;
var<workgroup> shares: array<atomic<u32>, SHARE_WORDS>;

fn rotr(word: u32, bits: u32) -> u32 {
    return (word >> bits) | (word << (32u - bits));
}

fn swap_bytes(word: u32) -> u32 {
    return ((word & 0x000000ffu) << 24u) | ((word & 0x0000ff00u) << 8u)
         | ((word & 0x00ff0000u) >> 8u) | ((word & 0xff000000u) >> 24u);
}

fn compress(state: Hash, words: array<u32, 16>) -> Hash {
    var schedule = words;
    var a = state[0];
    var b = state[1];
    var c = state[2];
    var d = state[3];
    var e = state[4];
    var f = state[5];
    var g = state[6];
    var h = state[7];
    for (var i = 0u; i < 64u; i++) {
        let slot = i & 15u;
        if i >= 16u {
            let x = schedule[(i - 15u) & 15u];
            let y = schedule[(i - 2u) & 15u];
            let sigma0 = rotr(x, 7u) ^ rotr(x, 18u) ^ (x >> 3u);
            let sigma1 = rotr(y, 17u) ^ rotr(y, 19u) ^ (y >> 10u);
            schedule[slot] += sigma0 + schedule[(i - 7u) & 15u] + sigma1;
        }
        let sum1 = rotr(e, 6u) ^ rotr(e, 11u) ^ rotr(e, 25u);
        let choice = (e & f) ^ (~e & g);
        let t1 = h + sum1 + choice + K[i] + schedule[slot];
        let sum0 = rotr(a, 2u) ^ rotr(a, 13u) ^ rotr(a, 22u);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let t2 = sum0 + majority;
        h = g;
        g = f;
        f = e;
        e = d + t1;
        d = c;
        c = b;
        b = a;
        a = t1 + t2;
    }
    return Hash(
        a + state[0], b + state[1], c + state[2], d + state[3],
        e + state[4], f + state[5], g + state[6], h + state[7],
    );
}

fn hash_nonce(nonce: u32) -> Hash {
    // Header bytes 64–79, SHA-256 padding, and the full header length in bits.
    let first = compress(job.midstate, array<u32, 16>(
        job.tail[0], job.tail[1], job.tail[2], swap_bytes(nonce),
        0x80000000u, 0u, 0u, 0u, 0u, 0u, 0u, 0u, 0u, 0u, 0u, 640u,
    ));
    let second = compress(IV, array<u32, 16>(
        first[0], first[1], first[2], first[3], first[4], first[5], first[6], first[7],
        0x80000000u, 0u, 0u, 0u, 0u, 0u, 0u, 256u,
    ));
    // Bitcoin compares the digest as a little-endian number. These words are most significant first.
    return Hash(
        swap_bytes(second[7]), swap_bytes(second[6]), swap_bytes(second[5]), swap_bytes(second[4]),
        swap_bytes(second[3]), swap_bytes(second[2]), swap_bytes(second[1]), swap_bytes(second[0]),
    );
}

fn compare(a: Hash, b: Hash) -> i32 {
    for (var i = 0u; i < 8u; i++) {
        if a[i] < b[i] { return -1; }
        if a[i] > b[i] { return 1; }
    }
    return 0;
}

@compute @workgroup_size(WORKGROUP_SIZE)
fn scan(
    @builtin(local_invocation_index) lane: u32,
    @builtin(global_invocation_id) id: vec3<u32>,
    @builtin(workgroup_id) group: vec3<u32>,
) {
    if lane < SHARE_WORDS {
        atomicStore(&shares[lane], 0u);
    }
    workgroupBarrier();

    let nonce = job.start + id.x;
    var hash = Hash(0xffffffffu, 0xffffffffu, 0xffffffffu, 0xffffffffu,
                    0xffffffffu, 0xffffffffu, 0xffffffffu, 0xffffffffu);
    if id.x < job.count {
        hash = hash_nonce(nonce);
        if compare(hash, job.threshold) <= 0 {
            atomicOr(&shares[lane / 32u], 1u << (lane % 32u));
        }
        if id.x == job.count - 1u {
            output.last = hash;
        }
    }
    candidates[lane] = Candidate(hash, nonce);
    workgroupBarrier();

    // Reduce to one minimum per workgroup, keeping the full hash to break ties.
    for (var stride = WORKGROUP_SIZE / 2u; stride > 0u; stride /= 2u) {
        if lane < stride {
            let other = candidates[lane + stride];
            if compare(other.hash, candidates[lane].hash) < 0 {
                candidates[lane] = other;
            }
        }
        workgroupBarrier();
    }
    if lane == 0u {
        output.groups[group.x].best = candidates[0];
    }
    if lane < SHARE_WORDS {
        output.groups[group.x].shares[lane] = atomicLoad(&shares[lane]);
    }
}
