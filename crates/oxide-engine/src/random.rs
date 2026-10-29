//! `java.util.Random`, bit-for-bit, so seeded eventual-consistency decisions match the Java
//! emulator's.

const MULTIPLIER: i64 = 0x5_DEEC_E66D;
const ADDEND: i64 = 0xB;
const MASK: i64 = (1 << 48) - 1;

#[derive(Clone, Debug)]
pub struct JavaRandom {
    seed: i64,
}

impl JavaRandom {
    pub fn new(seed: i64) -> Self {
        Self {
            seed: (seed ^ MULTIPLIER) & MASK,
        }
    }

    fn next(&mut self, bits: u32) -> i32 {
        self.seed = (self.seed.wrapping_mul(MULTIPLIER).wrapping_add(ADDEND)) & MASK;
        (self.seed >> (48 - bits)) as i32
    }

    pub fn next_int(&mut self) -> i32 {
        self.next(32)
    }

    /// `nextInt(bound)` for `bound > 0`.
    pub fn next_int_bounded(&mut self, bound: i32) -> i32 {
        assert!(bound > 0);
        let mut r = self.next(31);
        let m = bound - 1;
        if bound & m == 0 {
            return ((i64::from(bound) * i64::from(r)) >> 31) as i32;
        }
        let mut u = r;
        loop {
            r = u % bound;
            if u.wrapping_sub(r).wrapping_add(m) >= 0 {
                return r;
            }
            u = self.next(31);
        }
    }

    pub fn next_double(&mut self) -> f64 {
        let hi = i64::from(self.next(26)) << 27;
        let lo = i64::from(self.next(27));
        (hi + lo) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    pub fn next_float(&mut self) -> f32 {
        self.next(24) as f32 / (1 << 24) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_java() {
        // Reference values from a JDK 21 run.
        assert_eq!(JavaRandom::new(42).next_int(), -1170105035);
        assert_eq!(JavaRandom::new(42).next_int_bounded(100), 30);
        assert_eq!(JavaRandom::new(42).next_double(), 0.7275636800328681);
        assert_eq!(JavaRandom::new(0).next_int(), -1155484576);
        assert_eq!(JavaRandom::new(42).next_float(), 0.7275637);
        let mut r = JavaRandom::new(7);
        let got = (
            r.next_int_bounded(7),
            r.next_int_bounded(1000),
            r.next_double(),
        );
        assert_eq!(got, (3, 164, 0.7491696031336331));
    }
}
