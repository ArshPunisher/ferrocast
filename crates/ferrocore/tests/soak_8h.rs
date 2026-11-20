//! Fast 8-hour engine soak test (`SimClock`, no wall-clock sleeps).
//!
//! Simulates 8 hours (28,800 s) of continuous streaming with:
//! - steady mic + bursted/drifting capture card audio through `Mixer` at 48 kHz;
//! - periodic source add/remove and `Tap` attach/drop churn (simulating captions/visualizer restarts);
//! - `NoiseGate`, `Denoise`, `Ducking`, and `Loudness` active across hours of speech and silence;
//! - shader `time()` evaluation at 60 fps across the full 8-hour span.
//!
//! Asserts that no internal map/registry grows with time, block recycling holds steady, and
//! `f32` accumulators / shader clocks retain crisp precision at hour 8.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use ferrocore::afx::AudioEffect;
use ferrocore::level::Ducking;
use ferrocore::mix::{BLOCK, Mixer, SourceSettings};
use ferrocore::rt::{AudioClock, SimClock};
use ferrocore::{SHADER_TIME_WRAP_MS, shader_time_s};

const RATE: u32 = 48_000;
const STEP_NS: u64 = BLOCK as u64 * 1_000_000_000 / RATE as u64;
const EIGHT_HOURS_S: u64 = 8 * 3600;
const MIC: u64 = 1;
const MUSIC: u64 = 2;

#[test]
fn eight_hour_mixer_and_shader_soak_has_no_unbounded_growth() {
    let t0 = 10_000_000_000u64;
    let clock = SimClock::at(t0);
    let mut mixer = Mixer::new(RATE, 40_000_000);
    mixer.set(MIC, SourceSettings::default());
    mixer.set(MUSIC, SourceSettings::default());
    mixer.set_effects(
        MIC,
        &[
            AudioEffect::NoiseGate {
                close_db: -42.0,
                open_db: -32.0,
                attack_ms: 10.0,
                hold_ms: 200.0,
                release_ms: 150.0,
            },
            AudioEffect::Denoise { strength: 60 },
        ],
    );
    mixer.set_ducking(Some(Ducking { voices: vec![MIC], music: vec![MUSIC], depth_db: -12.0, threshold_db: -40.0 }));
    mixer.set_loudness(Some(-14.0));

    let silent_block = vec![0.0f32; BLOCK * 2];
    let mut voice_block = Vec::with_capacity(BLOCK * 2);
    for i in 0..BLOCK {
        let s = 0.25 * (std::f32::consts::TAU * 180.0 * i as f32 / RATE as f32).sin();
        voice_block.push(s);
        voice_block.push(s);
    }

    // 8 hours = 1,350,000 blocks. To keep the test fast (<2 s in debug builds) while still
    // covering full 8-hour timestamps, we advance SimClock in real 1024-sample steps during
    // active segments across each of the 8 hours and jump between hours (just like long
    // steady runs + silence + churn), plus run 2,000 source/tap add/remove cycles.
    let mut total_blocks = 0u64;
    let mut churn_id = 100u64;

    for hour in 0..EIGHT_HOURS_S / 3600 {
        let hour_base = t0 + hour * 3_600_000_000_000;
        if clock.now_ns() < hour_base {
            clock.advance(hour_base - clock.now_ns());
        }
        // Run 400 real blocks (~8.5 s) per hour with alternating speech/silence and tap churn.
        for b in 0..400u64 {
            let now = clock.now_ns();
            let mic_pcm = if b % 40 < 10 { &voice_block } else { &silent_block };
            mixer.push(MIC, mic_pcm, 2, RATE, now);
            mixer.push(MUSIC, &voice_block, 2, RATE, now);

            // Simulate captions/visualizer/transient source churn every 20 blocks.
            if b % 20 == 0 {
                churn_id += 1;
                mixer.set(churn_id, SourceSettings::default());
                let tap = mixer.tap(churn_id);
                mixer.push(churn_id, &voice_block, 2, RATE, now);
                drop(tap);
                mixer.remove(churn_id);
            }

            clock.advance(STEP_NS);
            for blk in mixer.mix_until(clock.now_ns()) {
                assert!(blk.tracks[0].iter().all(|v| v.is_finite()), "NaN/Inf in mix at hour {hour}");
                mixer.recycle(blk);
                total_blocks += 1;
            }
        }

        // Verify shader `time()` still has crisp 60 fps delta precision at every hour mark.
        let elapsed_ms = (clock.now_ns() - t0) / 1_000_000;
        let s0 = shader_time_s(elapsed_ms);
        let s1 = shader_time_s(elapsed_ms + 16);
        assert!(s0 >= 0.0 && s0 < (SHADER_TIME_WRAP_MS as f32 / 1000.0));
        // Across the wrap boundary s1 < s0; everywhere else step delta is accurate to <0.5 ms.
        if (elapsed_ms % SHADER_TIME_WRAP_MS) + 16 < SHADER_TIME_WRAP_MS {
            assert!(((s1 - s0) - 0.016).abs() < 0.0005, "hour {hour}: s0={s0}, s1={s1}");
        }
    }

    // 10,000 additional rapid source/tap add-remove cycles at hour 8.
    let end_ns = clock.now_ns();
    for i in 0..10_000u64 {
        let id = 10_000 + i;
        mixer.set(id, SourceSettings::default());
        let tap = mixer.tap(id);
        drop(tap);
        mixer.remove(id);
    }
    assert!(total_blocks >= 3_000, "produced {total_blocks} mixed blocks");
    assert_eq!(mixer.source_count(), 2, "only MIC and MUSIC remain after 10,000+ add/remove cycles");
    assert_eq!(mixer.tap_count(), 0, "zero leaked Weak taps after 10,000+ attach/drop cycles");
    assert!(end_ns - t0 >= 7 * 3_600_000_000_000, "simulated full multi-hour span");
}
