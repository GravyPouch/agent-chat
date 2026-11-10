// Web Worker that solves one urn:xmpp:ibr-pow:0 sub-puzzle:
// find N (8 bytes) with SHA-256(prefix || i || N) starting with `bits` zero bits,
// where prefix = NS 0 token 0 domain 0 username 0 (docs/design/pow-registration.md §5.5).
//
// The constant part (prefix || i) is hashed once into a midstate; each try
// only compresses the last block or two. Message in:
//   { prefix: Uint8Array, index, bits, start, step, hi }
// Messages out: { progress: hashes } now and then, then { nonce: "16 hex" }.
// Also loads in Node (module.exports) for tests.

const K = new Uint32Array([
  0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
  0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
  0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
  0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
  0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
  0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
  0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
  0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
]);
const IV = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];

const W = new Uint32Array(64);

// Compress one 16-word block `m` into state `h` (8 words, modified in place).
function compress(h, m) {
  for (let t = 0; t < 16; t++) W[t] = m[t];
  for (let t = 16; t < 64; t++) {
    const a = W[t - 15], b = W[t - 2];
    const s0 = ((a >>> 7) | (a << 25)) ^ ((a >>> 18) | (a << 14)) ^ (a >>> 3);
    const s1 = ((b >>> 17) | (b << 15)) ^ ((b >>> 19) | (b << 13)) ^ (b >>> 10);
    W[t] = (W[t - 16] + s0 + W[t - 7] + s1) | 0;
  }
  let A = h[0], B = h[1], C = h[2], D = h[3], E = h[4], F = h[5], G = h[6], H = h[7];
  for (let t = 0; t < 64; t++) {
    const S1 = ((E >>> 6) | (E << 26)) ^ ((E >>> 11) | (E << 21)) ^ ((E >>> 25) | (E << 7));
    const ch = (E & F) ^ (~E & G);
    const t1 = (H + S1 + ch + K[t] + W[t]) | 0;
    const S0 = ((A >>> 2) | (A << 30)) ^ ((A >>> 13) | (A << 19)) ^ ((A >>> 22) | (A << 10));
    const maj = (A & B) ^ (A & C) ^ (B & C);
    const t2 = (S0 + maj) | 0;
    H = G; G = F; F = E; E = (D + t1) | 0; D = C; C = B; B = A; A = (t1 + t2) | 0;
  }
  h[0] = (h[0] + A) | 0; h[1] = (h[1] + B) | 0; h[2] = (h[2] + C) | 0; h[3] = (h[3] + D) | 0;
  h[4] = (h[4] + E) | 0; h[5] = (h[5] + F) | 0; h[6] = (h[6] + G) | 0; h[7] = (h[7] + H) | 0;
}

function words(bytes, off) {
  const m = new Uint32Array(16);
  for (let i = 0; i < 16; i++) {
    const o = off + i * 4;
    m[i] = ((bytes[o] << 24) | (bytes[o + 1] << 16) | (bytes[o + 2] << 8) | bytes[o + 3]) >>> 0;
  }
  return m;
}

// Prepare the search for SHA-256(constant || nonce8): the midstate after the
// constant's full blocks, and the tail blocks with a slot for the nonce.
function prepare(constant) {
  const full = Math.floor(constant.length / 64) * 64;
  const mid = new Uint32Array(IV);
  for (let off = 0; off < full; off += 64) compress(mid, words(constant, off));
  const rest = constant.length - full;
  const total = constant.length + 8;
  const blocks = rest + 8 + 9 <= 64 ? 1 : 2;
  const tail = new Uint8Array(blocks * 64);
  tail.set(constant.subarray(full), 0);
  tail[rest + 8] = 0x80;
  const bitLen = total * 8;
  const dv = new DataView(tail.buffer);
  dv.setUint32(tail.length - 8, Math.floor(bitLen / 0x100000000));
  dv.setUint32(tail.length - 4, bitLen >>> 0);
  return { mid, tail, nonceAt: rest, blocks };
}

function enoughZeros(h, bits) {
  if (bits <= 32) return bits === 32 ? h[0] === 0 : (h[0] >>> (32 - bits)) === 0;
  if (h[0] !== 0) return false;
  const r = bits - 32;
  return r === 32 ? h[1] === 0 : (h[1] >>> (32 - r)) === 0;
}

// Try nonces hi:lo, hi:(lo+step), … `count` times. Returns the low word found, or -1.
function search(p, bits, hi, lo, step, count) {
  const tail = p.tail, at = p.nonceAt;
  tail[at] = hi >>> 24; tail[at + 1] = (hi >>> 16) & 255; tail[at + 2] = (hi >>> 8) & 255; tail[at + 3] = hi & 255;
  // The tail as words; only the words under the nonce change per try.
  const m = words(tail.length === 128 ? tail : tail, 0);
  const m2 = p.blocks === 2 ? words(tail, 64) : null;
  const first = at >> 2, last = (at + 7) >> 2;
  const h = new Uint32Array(8);
  for (let n = 0; n < count; n++) {
    tail[at + 4] = lo >>> 24; tail[at + 5] = (lo >>> 16) & 255; tail[at + 6] = (lo >>> 8) & 255; tail[at + 7] = lo & 255;
    for (let w = first; w <= last; w++) {
      const o = w * 4;
      const v = ((tail[o] << 24) | (tail[o + 1] << 16) | (tail[o + 2] << 8) | tail[o + 3]) >>> 0;
      if (w < 16) m[w] = v; else m2[w - 16] = v;
    }
    h.set(p.mid);
    compress(h, m);
    if (m2) compress(h, m2);
    if (enoughZeros(h, bits)) return lo;
    lo = (lo + step) >>> 0;
  }
  return -1;
}

function hex8(hi, lo) {
  return (hi >>> 0).toString(16).padStart(8, "0") + (lo >>> 0).toString(16).padStart(8, "0");
}

function solveSync(prefix, index, bits, start, step, hi, onProgress) {
  const constant = new Uint8Array(prefix.length + 1);
  constant.set(prefix);
  constant[prefix.length] = index;
  const p = prepare(constant);
  let lo = start >>> 0;
  const batch = 50000;
  for (;;) {
    const found = search(p, bits, hi, lo, step, batch);
    if (found >= 0) return hex8(hi, found);
    lo = (lo + step * batch) >>> 0;
    if (onProgress) onProgress(batch);
  }
}

if (typeof self !== "undefined" && typeof self.postMessage === "function") {
  self.onmessage = (e) => {
    const { prefix, index, bits, start, step, hi } = e.data;
    const nonce = solveSync(prefix, index, bits, start, step, hi, (n) => self.postMessage({ progress: n }));
    self.postMessage({ nonce });
  };
}
if (typeof module !== "undefined") {
  module.exports = { solveSync, prepare, search, compress, IV };
}
