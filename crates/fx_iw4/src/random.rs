/// 2: SplitMix64 mixing in place of BLAKE3 (same inputs, new values).
pub const FX_RANDOM_VERSION: u32 = 2;
#[derive(Copy, Clone, Debug)]
#[repr(u32)]
pub enum FxRandomChannel {
    AngularPitch = 1,
    AngularRoll = 2,
    AngularYaw = 3,
    Atlas = 4,
    Color = 5,
    Delay = 6,
    EmitDistance = 7,
    GlassCrack = 8,
    GlassShatter = 9,
    Gravity = 10,
    InitialRotation = 11,
    Life = 12,
    OneShotCount = 13,
    Reflection = 14,
    RotationDelta = 15,
    Scale = 16,
    Size = 17,
    SpawnAngleYaw = 18,
    SpawnHeight = 19,
    SpawnOriginX = 20,
    SpawnOriginY = 21,
    SpawnOriginZ = 22,
    SpawnPitch = 23,
    SpawnRadius = 24,
    SpawnRoll = 25,
    SpawnYaw = 26,
    VelocityX = 27,
    VelocityY = 28,
    VelocityZ = 29,
    Visual = 30,
}

// Domain separators: the same words drawn for different purposes must not
// give the same values.
const EFFECT_DOMAIN: u64 = 0x6566_6665_6374_0001;
const ELEMENT_DOMAIN: u64 = 0x656c_656d_656e_0002;
const TRAIL_DOMAIN: u64 = 0x7472_6169_6c00_0003;
const SAMPLE_DOMAIN: u64 = 0x7361_6d70_6c65_0004;
const GOLDEN: u64 = 0x9e37_79b9_7f4a_7c15;

/// SplitMix64's finalizer: every input bit reaches every output bit.
#[inline]
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// One well-mixed word from `words` under `domain`: a deterministic random
/// draw. These run for every particle's every channel each frame; they were
/// BLAKE3 hashes of the same tuple, which in the browser (no SIMD there)
/// cost about 4% of a busy frame on its own.
#[inline]
fn digest_words(domain: u64, words: &[u64]) -> u64 {
    let mut h = mix(domain ^ u64::from(FX_RANDOM_VERSION).wrapping_mul(GOLDEN));
    for &word in words {
        h = mix(h ^ mix(word.wrapping_add(GOLDEN)));
    }
    h
}

pub fn effect_random_key(instance: u64, begin_msec: i32) -> u64 {
    digest_words(EFFECT_DOMAIN, &[instance, u64::from(begin_msec as u32)])
}

pub fn elem_random_seed(effect_key: u64, sequence: u8, begin_msec: i32) -> u64 {
    digest_words(
        ELEMENT_DOMAIN,
        &[effect_key, u64::from(sequence), u64::from(begin_msec as u32)],
    )
}

pub fn trail_random_seed(effect_key: u64, sequence: i8) -> u64 {
    digest_words(TRAIL_DOMAIN, &[effect_key, u64::from(sequence as u8)])
}

fn sample_word(key: u64, channel: FxRandomChannel, index: u64) -> u64 {
    digest_words(SAMPLE_DOMAIN, &[key, channel as u64, index])
}

pub fn sample_u16(key: u64, channel: FxRandomChannel) -> u16 {
    (sample_word(key, channel, 0) >> 48) as u16
}

pub fn sample_f32(key: u64, channel: FxRandomChannel) -> f32 {
    sample_at(key, channel, 0)
}

pub fn sample_at(key: u64, channel: FxRandomChannel, index: u64) -> f32 {
    (sample_word(key, channel, index) >> 40) as f32 / 16_777_216.0
}

pub fn elem_visual_index(count: u8, key: u64) -> usize {
    if count <= 1 {
        return 0;
    }
    ((u32::from(sample_u16(key, FxRandomChannel::Visual)) * u32::from(count)) >> 16) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The draws must look uniform and independent across channels and
    /// indices: bucket counts and adjacent-pair correlation over many keys.
    #[test]
    fn draws_are_uniform_and_uncorrelated() {
        const N: u64 = 200_000;
        let mut buckets = [0u32; 16];
        let mut sum_xy = 0.0f64;
        let mut sum_x = 0.0f64;
        let mut sum_x2 = 0.0f64;
        for i in 0..N {
            let key = elem_random_seed(effect_random_key(i / 7, (i % 7) as i32), (i % 13) as u8, 0);
            let a = f64::from(sample_f32(key, FxRandomChannel::Size));
            let b = f64::from(sample_at(key, FxRandomChannel::Size, 1));
            assert!((0.0..1.0).contains(&a));
            buckets[(a * 16.0) as usize] += 1;
            sum_x += a;
            sum_x2 += a * a;
            sum_xy += a * b;
        }
        let n = N as f64;
        let expected = n / 16.0;
        for count in buckets {
            assert!((f64::from(count) - expected).abs() < expected * 0.03, "{buckets:?}");
        }
        let mean = sum_x / n;
        let var = sum_x2 / n - mean * mean;
        let cov = sum_xy / n - mean * mean;
        assert!((cov / var).abs() < 0.01, "correlation {}", cov / var);
        assert_ne!(
            sample_u16(1, FxRandomChannel::Color),
            sample_u16(1, FxRandomChannel::Size)
        );
    }
}
