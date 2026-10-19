//! Bit-compatible port of NumPy's `default_rng` (PCG64 + SeedSequence) plus
//! the small subset of `Generator` methods the texture pipeline uses.
//!
//! The texture is seeded from the cover bytes, so keeping the generator
//! identical means the same cover always produces the same texture — not just
//! the same style.

const MASK64: u64 = u64::MAX;
const MASK128: u128 = u128::MAX;
const HASH_INIT_A: u32 = 0x43b0_d7e5;
const MULT_A: u32 = 0x931e_8875;
const HASH_INIT_B: u32 = 0x8b51_f9dd;
const MULT_B: u32 = 0x58f3_8ded;
const MIX_MULT_L: u32 = 0xca01_f9dd;
const MIX_MULT_R: u32 = 0x4973_f715;
const PCG_MULT: u128 = 0x2360_ed05_1fc6_5da4_4385_df64_9fcc_f645;

#[inline]
fn hashmix(mut value: u32, hash_const: &mut u32) -> u32 {
    value ^= *hash_const;
    *hash_const = hash_const.wrapping_mul(MULT_A);
    value = value.wrapping_mul(*hash_const);
    value ^= value >> 16;
    value
}

#[inline]
fn mix(x: u32, y: u32) -> u32 {
    let mut r = MIX_MULT_L
        .wrapping_mul(x)
        .wrapping_sub(MIX_MULT_R.wrapping_mul(y));
    r ^= r >> 16;
    r
}

/// Port of `np.random.SeedSequence(entropy).generate_state(8, uint32)`.
fn seed_state(entropy: u64) -> [u64; 4] {
    let ent = [entropy as u32, (entropy >> 32) as u32];

    let mut hc = HASH_INIT_A;
    let mut mixer = [0u32; 4];
    for (i, m) in mixer.iter_mut().enumerate() {
        let v = if i < ent.len() { ent[i] } else { 0 };
        *m = hashmix(v, &mut hc);
    }
    for src in 0..4 {
        for dst in 0..4 {
            if src != dst {
                let h = hashmix(mixer[src], &mut hc);
                mixer[dst] = mix(mixer[dst], h);
            }
        }
    }

    let mut hc = HASH_INIT_B;
    let mut words = [0u32; 8];
    for (i, w) in words.iter_mut().enumerate() {
        let mut d = mixer[i % 4] ^ hc;
        hc = hc.wrapping_mul(MULT_B);
        d = d.wrapping_mul(hc);
        d ^= d >> 16;
        *w = d;
    }
    [
        words[0] as u64 | ((words[1] as u64) << 32),
        words[2] as u64 | ((words[3] as u64) << 32),
        words[4] as u64 | ((words[5] as u64) << 32),
        words[6] as u64 | ((words[7] as u64) << 32),
    ]
}

pub struct Rng {
    state: u128,
    inc: u128,
    has_uint32: bool,
    uinteger: u32,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        let w = seed_state(seed);
        let initstate = ((w[0] as u128) << 64) | w[1] as u128;
        let initseq = ((w[2] as u128) << 64) | w[3] as u128;
        let mut rng = Rng {
            state: 0,
            inc: ((initseq << 1) & MASK128) | 1,
            has_uint32: false,
            uinteger: 0,
        };
        rng.step();
        rng.state = rng.state.wrapping_add(initstate);
        rng.step();
        rng
    }

    #[inline]
    fn step(&mut self) {
        self.state = self.state.wrapping_mul(PCG_MULT).wrapping_add(self.inc);
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.step();
        let hi = (self.state >> 64) as u64;
        let lo = self.state as u64;
        let v = hi ^ lo;
        let rot = (hi >> 58) as u32;
        v.rotate_right(rot)
    }

    #[inline]
    pub fn next_u32(&mut self) -> u32 {
        if self.has_uint32 {
            self.has_uint32 = false;
            return self.uinteger;
        }
        let n = self.next_u64();
        self.has_uint32 = true;
        self.uinteger = (n >> 32) as u32;
        n as u32
    }

    #[inline]
    fn lemire32(&mut self, rng: u32) -> u32 {
        let rng_excl = rng as u64 + 1;
        loop {
            let m = self.next_u32() as u64 * rng_excl;
            let leftover = m as u32;
            if leftover < rng_excl as u32 {
                let threshold = ((u32::MAX - rng) as u64 % rng_excl) as u32;
                if leftover < threshold {
                    continue;
                }
            }
            return (m >> 32) as u32;
        }
    }

    #[inline]
    fn lemire64(&mut self, rng: u64) -> u64 {
        let rng_excl = rng as u128 + 1;
        loop {
            let m = self.next_u64() as u128 * rng_excl;
            let leftover = m as u64;
            if leftover < rng_excl as u64 {
                let threshold = ((MASK64 - rng) as u128 % rng_excl) as u64;
                if leftover < threshold {
                    continue;
                }
            }
            return (m >> 64) as u64;
        }
    }

    /// `random_bounded_uint64(off, rng)` (closed interval), use_masked = false.
    #[inline]
    pub fn bounded(&mut self, off: u64, rng: u64) -> u64 {
        if rng == 0 {
            return off;
        }
        if rng <= 0xFFFF_FFFF {
            if rng == 0xFFFF_FFFF {
                return off + self.next_u32() as u64;
            }
            return off + self.lemire32(rng as u32) as u64;
        }
        if rng == MASK64 {
            return off + self.next_u64();
        }
        off + self.lemire64(rng)
    }

    /// `Generator.integers(0, high)` scalar.
    #[inline]
    pub fn below(&mut self, high: u64) -> u64 {
        self.bounded(0, high - 1)
    }

    /// `Generator.integers(0, high, size=n)`.
    pub fn integers(&mut self, high: u64, out: &mut [u64]) {
        let rng = high - 1;
        for o in out.iter_mut() {
            *o = self.bounded(0, rng);
        }
    }

    /// `Generator.choice(a, size, replace=False)` where `a` is `0..pop_size`.
    pub fn choice_without_replacement(&mut self, pop_size: usize, size: usize) -> Vec<usize> {
        let mut idx = vec![0usize; size];
        let mask = gen_mask((size as f64 * 1.2) as u64);
        let set_size = 1 + mask as usize;
        let mut hash_set = vec![u64::MAX; set_size];
        for j in (pop_size - size)..pop_size {
            let val = self.bounded(0, j as u64);
            let mut loc = (val & mask) as usize;
            while hash_set[loc] != u64::MAX && hash_set[loc] != val {
                loc = (loc + 1) & mask as usize;
            }
            if hash_set[loc] == u64::MAX {
                hash_set[loc] = val;
                idx[j - (pop_size - size)] = val as usize;
            } else {
                let mut loc = j & mask as usize;
                while hash_set[loc] != u64::MAX {
                    loc = (loc + 1) & mask as usize;
                }
                hash_set[loc] = j as u64;
                idx[j - (pop_size - size)] = j;
            }
        }
        for i in (1..size).rev() {
            let j = self.bounded(0, i as u64) as usize;
            idx.swap(i, j);
        }
        idx
    }
}

#[inline]
fn gen_mask(v: u64) -> u64 {
    let mut v = v;
    v |= v >> 1;
    v |= v >> 2;
    v |= v >> 4;
    v |= v >> 8;
    v |= v >> 16;
    v |= v >> 32;
    v
}
