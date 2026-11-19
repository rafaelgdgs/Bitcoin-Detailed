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

alias Block = array<u32, 16>;

override WORKGROUP_SIZE: u32 = 128u;

struct Job {
    midstate: Hash,
    after_three_rounds: Hash,
    tail: array<u32, 3>,
    start: u32,
    count: u32,
    threshold: Hash,
}

struct Output {
    last: Hash,
    flags: array<atomic<u32>>,
}

@group(0) @binding(0) var<storage, read> job: Job;
@group(0) @binding(1) var<storage, read_write> output: Output;

fn rotr(word: u32, bits: u32) -> u32 {
    return (word >> bits) | (word << (32u - bits));
}

fn swap_bytes(word: u32) -> u32 {
    return ((word & 0x000000ffu) << 24u) | ((word & 0x0000ff00u) << 8u)
         | ((word & 0x00ff0000u) >> 8u) | ((word & 0xff000000u) >> 24u);
}

fn round(state: Hash, kw: u32) -> Hash {
    let a = state[0];
    let b = state[1];
    let c = state[2];
    let e = state[4];
    let f = state[5];
    let g = state[6];
    let sum1 = rotr(e, 6u) ^ rotr(e, 11u) ^ rotr(e, 25u);
    let choice = (e & f) ^ (~e & g);
    let t1 = state[7] + sum1 + choice + kw;
    let sum0 = rotr(a, 2u) ^ rotr(a, 13u) ^ rotr(a, 22u);
    let majority = (a & b) ^ (a & c) ^ (b & c);
    return Hash(t1 + sum0 + majority, a, b, c, state[3] + t1, e, f, g);
}

fn schedule_word(w0: u32, w1: u32, w9: u32, w14: u32) -> u32 {
    let sigma0 = rotr(w1, 7u) ^ rotr(w1, 18u) ^ (w1 >> 3u);
    let sigma1 = rotr(w14, 17u) ^ rotr(w14, 19u) ^ (w14 >> 10u);
    return w0 + sigma0 + w9 + sigma1;
}

fn expanded_rounds(start: Hash, words: Block) -> Hash {
    var state = start;
    var w = words;
    // SHA-256 needs the previous 16 schedule words. Unrolling one rotation of the
    // ring keeps each private array access constant, so the words can stay in registers.
    for (var base = 16u; base < 64u; base += 16u) {
        w[0] = schedule_word(w[0], w[1], w[9], w[14]);
        state = round(state, K[base] + w[0]);
        w[1] = schedule_word(w[1], w[2], w[10], w[15]);
        state = round(state, K[base + 1u] + w[1]);
        w[2] = schedule_word(w[2], w[3], w[11], w[0]);
        state = round(state, K[base + 2u] + w[2]);
        w[3] = schedule_word(w[3], w[4], w[12], w[1]);
        state = round(state, K[base + 3u] + w[3]);
        w[4] = schedule_word(w[4], w[5], w[13], w[2]);
        state = round(state, K[base + 4u] + w[4]);
        w[5] = schedule_word(w[5], w[6], w[14], w[3]);
        state = round(state, K[base + 5u] + w[5]);
        w[6] = schedule_word(w[6], w[7], w[15], w[4]);
        state = round(state, K[base + 6u] + w[6]);
        w[7] = schedule_word(w[7], w[8], w[0], w[5]);
        state = round(state, K[base + 7u] + w[7]);
        w[8] = schedule_word(w[8], w[9], w[1], w[6]);
        state = round(state, K[base + 8u] + w[8]);
        w[9] = schedule_word(w[9], w[10], w[2], w[7]);
        state = round(state, K[base + 9u] + w[9]);
        w[10] = schedule_word(w[10], w[11], w[3], w[8]);
        state = round(state, K[base + 10u] + w[10]);
        w[11] = schedule_word(w[11], w[12], w[4], w[9]);
        state = round(state, K[base + 11u] + w[11]);
        w[12] = schedule_word(w[12], w[13], w[5], w[10]);
        state = round(state, K[base + 12u] + w[12]);
        w[13] = schedule_word(w[13], w[14], w[6], w[11]);
        state = round(state, K[base + 13u] + w[13]);
        w[14] = schedule_word(w[14], w[15], w[7], w[12]);
        state = round(state, K[base + 14u] + w[14]);
        w[15] = schedule_word(w[15], w[0], w[8], w[13]);
        state = round(state, K[base + 15u] + w[15]);
    }
    return state;
}

fn add_state(a: Hash, b: Hash) -> Hash {
    return Hash(a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3],
                a[4] + b[4], a[5] + b[5], a[6] + b[6], a[7] + b[7]);
}

fn hash_nonce(nonce: u32) -> Hash {
    // Header bytes 64–79, SHA-256 padding, and the full header length in bits.
    let header = Block(job.tail[0], job.tail[1], job.tail[2], swap_bytes(nonce),
                       0x80000000u, 0u, 0u, 0u, 0u, 0u, 0u, 0u, 0u, 0u, 0u, 640u);
    // Rounds 0–2 use fixed words. Round 3 is the first to use the nonce.
    var state = job.after_three_rounds;
    state = round(state, K[3] + header[3]);
    state = round(state, K[4] + header[4]);
    state = round(state, K[5]);
    state = round(state, K[6]);
    state = round(state, K[7]);
    state = round(state, K[8]);
    state = round(state, K[9]);
    state = round(state, K[10]);
    state = round(state, K[11]);
    state = round(state, K[12]);
    state = round(state, K[13]);
    state = round(state, K[14]);
    state = round(state, K[15] + header[15]);
    let first = add_state(job.midstate, expanded_rounds(state, header));

    let digest = Block(first[0], first[1], first[2], first[3], first[4], first[5], first[6], first[7],
                       0x80000000u, 0u, 0u, 0u, 0u, 0u, 0u, 256u);
    state = IV;
    state = round(state, K[0] + digest[0]);
    state = round(state, K[1] + digest[1]);
    state = round(state, K[2] + digest[2]);
    state = round(state, K[3] + digest[3]);
    state = round(state, K[4] + digest[4]);
    state = round(state, K[5] + digest[5]);
    state = round(state, K[6] + digest[6]);
    state = round(state, K[7] + digest[7]);
    state = round(state, K[8] + digest[8]);
    state = round(state, K[9]);
    state = round(state, K[10]);
    state = round(state, K[11]);
    state = round(state, K[12]);
    state = round(state, K[13]);
    state = round(state, K[14]);
    state = round(state, K[15] + digest[15]);
    return add_state(IV, expanded_rounds(state, digest));
}

fn bitcoin_order(digest: Hash) -> Hash {
    return Hash(
        swap_bytes(digest[7]), swap_bytes(digest[6]), swap_bytes(digest[5]), swap_bytes(digest[4]),
        swap_bytes(digest[3]), swap_bytes(digest[2]), swap_bytes(digest[1]), swap_bytes(digest[0]),
    );
}

fn meets_threshold(digest: Hash) -> bool {
    // The most significant word decides almost all comparisons.
    let high = swap_bytes(digest[7]);
    if high != job.threshold[0] {
        return high < job.threshold[0];
    }
    let hash = bitcoin_order(digest);
    for (var i = 1u; i < 8u; i++) {
        if hash[i] != job.threshold[i] {
            return hash[i] < job.threshold[i];
        }
    }
    return true;
}

@compute @workgroup_size(WORKGROUP_SIZE)
fn scan(@builtin(global_invocation_id) id: vec3<u32>) {
    if id.x >= job.count {
        return;
    }
    let digest = hash_nonce(job.start + id.x);
    if meets_threshold(digest) {
        atomicOr(&output.flags[id.x / 32u], 1u << (id.x % 32u));
    }
    if id.x == job.count - 1u {
        output.last = bitcoin_order(digest);
    }
}
