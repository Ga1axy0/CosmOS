//! Minimal cryptographic PRNG seeded from available time-based entropy.
//!
//! This is a small, self-contained ChaCha20-based generator intended as a
//! minimal kernel-side `getrandom` backend for experimentation. It mixes in
//! low-quality entropy sources (timer/rtc jitter) and exposes a wait queue for
//! callers that want to block until the generator is seeded.

use core::convert::TryInto;

use lazy_static::lazy_static;

use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::task::{WaitQueue, WaitReason};
use crate::timer::get_time_ns;

/// ChaCha20 constants ("expand 32-byte k")
const CHACHA_CONST: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

struct RandomState {
    key: [u8; 32],
    nonce: [u8; 12],
    counter: u64,
    seeded: bool,
}

impl RandomState {
    const fn new() -> Self {
        Self {
            key: [0u8; 32],
            nonce: [0u8; 12],
            counter: 0,
            seeded: false,
        }
    }

    fn add_entropy(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        // Simple mixing: XOR input into key bytes (rotating) and XOR low bits
        // of current monotonic time.  This is intentionally small — in a real
        // kernel use a proper KDF/HMAC-DRBG to mix entropy.
        for (i, b) in self.key.iter_mut().enumerate() {
            *b ^= data[i % data.len()];
        }
        let t = get_time_ns();
        let tbytes = t.to_le_bytes();
        for i in 0..tbytes.len() {
            self.key[i % self.key.len()] ^= tbytes[i];
        }
        // Mark seeded once we have seen any entropy.
        self.seeded = true;
    }

    fn chacha20_block(key: &[u8; 32], counter: u32, nonce: &[u8; 12]) -> [u8; 64] {
        // state: constants | key | counter | nonce
        let mut state = [0u32; 16];
        state[0..4].copy_from_slice(&CHACHA_CONST);
        for i in 0..8 {
            let off = i * 4;
            state[4 + i] = u32::from_le_bytes(key[off..off + 4].try_into().unwrap());
        }
        state[12] = counter;
        state[13] = u32::from_le_bytes(nonce[0..4].try_into().unwrap());
        state[14] = u32::from_le_bytes(nonce[4..8].try_into().unwrap());
        state[15] = u32::from_le_bytes(nonce[8..12].try_into().unwrap());

        let mut working = state;

        // 20 rounds (10 double-rounds)
        for _ in 0..10 {
            // column rounds
            Self::quarter_round(&mut working, 0, 4, 8, 12);
            Self::quarter_round(&mut working, 1, 5, 9, 13);
            Self::quarter_round(&mut working, 2, 6, 10, 14);
            Self::quarter_round(&mut working, 3, 7, 11, 15);
            // diagonal rounds
            Self::quarter_round(&mut working, 0, 5, 10, 15);
            Self::quarter_round(&mut working, 1, 6, 11, 12);
            Self::quarter_round(&mut working, 2, 7, 8, 13);
            Self::quarter_round(&mut working, 3, 4, 9, 14);
        }

        for i in 0..16 {
            working[i] = working[i].wrapping_add(state[i]);
        }

        let mut out = [0u8; 64];
        for (i, word) in working.iter().enumerate() {
            let bytes = word.to_le_bytes();
            out[i * 4..i * 4 + 4].copy_from_slice(&bytes);
        }
        out
    }

    #[inline]
    fn quarter_round(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
        s[a] = s[a].wrapping_add(s[b]);
        s[d] ^= s[a];
        s[d] = s[d].rotate_left(16);

        s[c] = s[c].wrapping_add(s[d]);
        s[b] ^= s[c];
        s[b] = s[b].rotate_left(12);

        s[a] = s[a].wrapping_add(s[b]);
        s[d] ^= s[a];
        s[d] = s[d].rotate_left(8);

        s[c] = s[c].wrapping_add(s[d]);
        s[b] ^= s[c];
        s[b] = s[b].rotate_left(7);
    }

    fn fill_bytes(&mut self, mut out: &mut [u8]) {
        while !out.is_empty() {
            let block =
                Self::chacha20_block(&self.key, (self.counter & 0xffff_ffff) as u32, &self.nonce);
            self.counter = self.counter.wrapping_add(1);
            let take = core::cmp::min(out.len(), 64);
            out[..take].copy_from_slice(&block[..take]);
            // rekey: fold first 32 bytes of block into key for forward secrecy
            for i in 0..32 {
                self.key[i] ^= block[i];
            }
            out = &mut out[take..];
        }
    }
}

lazy_static! {
    static ref RNG: SpinNoIrqLock<RandomState> = SpinNoIrqLock::new(RandomState::new());
    static ref WAIT_QUEUE: WaitQueue = WaitQueue::new();
}

/// Add external entropy into the pool.  Callers should pass any collected
/// jitter/measurements as bytes.  If this is the first entropy, waiters are
/// woken.
pub fn add_entropy(data: &[u8]) {
    let mut rng = RNG.lock();
    let was_seeded = rng.seeded;
    rng.add_entropy(data);
    if !was_seeded && rng.seeded {
        // Wake waiters who were blocked waiting for initial seed.
        WAIT_QUEUE.wake_all();
    }
}

/// Mix a timestamp-derived seed (convenience wrapper).  Typically used by
/// early drivers (RTC) to seed as soon as a time source is available.
pub fn add_time_entropy() {
    let t = get_time_ns();
    add_entropy(&t.to_le_bytes());
}

/// Return whether the generator has been seeded at least once.
pub fn is_seeded() -> bool {
    RNG.lock().seeded
}

/// Block until seeded (if `blocking`), or return EAGAIN if non-blocking.
pub fn wait_for_seed(blocking: bool) -> Result<(), ERRNO> {
    if is_seeded() {
        return Ok(());
    }
    if !blocking {
        return Err(ERRNO::EAGAIN);
    }
    // Enqueue and sleep until a peer calls `add_entropy` and wakes us.
    WAIT_QUEUE.wait_with_reason_or_skip(WaitReason::Unknown, || is_seeded());
    Ok(())
}

/// Fill the provided buffer with cryptographically-derived bytes.  This
/// requires that the generator is seeded; callers should call `wait_for_seed`
/// if they want blocking semantics.
pub fn fill_bytes(buf: &mut [u8]) {
    let mut rng = RNG.lock();
    rng.fill_bytes(buf)
}
