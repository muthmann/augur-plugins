//! Verifies that the A1 protocol minima fit the production photodiode ring at
//! the 500 kSa/s bench rate, with the cache length left at its default.

use stage_a_plugin_contract::protocol::parse_csv;

use super::{SharedState, DEFAULT_CACHE_SECONDS, RING_MAX_SAMPLES};

const BENCH_RATE_HZ: u32 = 500_000;

#[test]
fn shipped_a1_protocols_retain_two_cycles_at_their_lowest_frequency() {
    let fixtures = [
        (
            "a1_lux_dark_offset.csv",
            include_str!("../../stage-a-a1/protocols/a1_lux_dark_offset.csv"),
        ),
        (
            "a1_illuminated_smoke.csv",
            include_str!("../../stage-a-a1/protocols/a1_illuminated_smoke.csv"),
        ),
        (
            "a1_fc_flux_discriminator.csv",
            include_str!("../../stage-a-a1/protocols/a1_fc_flux_discriminator.csv"),
        ),
        (
            "a1_triage_90min.csv",
            include_str!("../../stage-a-a1/protocols/a1_triage_90min.csv"),
        ),
        (
            "a1_stufe1_bode_dc.csv",
            include_str!("../../stage-a-a1/protocols/a1_stufe1_bode_dc.csv"),
        ),
        (
            "a1_stufe2_bode_u010.csv",
            include_str!("../../stage-a-a1/protocols/a1_stufe2_bode_u010.csv"),
        ),
        (
            "a1_stufe2_bode_u045.csv",
            include_str!("../../stage-a-a1/protocols/a1_stufe2_bode_u045.csv"),
        ),
        (
            "a1_stufe2_flussleiter.csv",
            include_str!("../../stage-a-a1/protocols/a1_stufe2_flussleiter.csv"),
        ),
    ];

    for (name, csv) in fixtures {
        let protocol = parse_csv(csv).unwrap_or_else(|error| panic!("{name}: {error}"));
        let lowest_hz = protocol
            .points
            .iter()
            .map(|point| point.frequency_hz)
            .fold(f64::INFINITY, f64::min);
        let required_samples = (2.0 * f64::from(BENCH_RATE_HZ) / lowest_hz).ceil() as usize;

        // The operator sets nothing: the ring sizes itself from the marker
        // period once the drive has stamped two phase-0 markers at the file's
        // lowest rung.
        let mut ring = SharedState {
            cache_seconds: DEFAULT_CACHE_SECONDS,
            rate_hz: BENCH_RATE_HZ,
            ..SharedState::default()
        };
        let period = (f64::from(BENCH_RATE_HZ) / lowest_hz) as u64;
        ring.push_marker(0);
        ring.push_marker(period);

        let capacity = ring.ring_capacity(BENCH_RATE_HZ);
        assert!(capacity <= RING_MAX_SAMPLES);
        assert!(
            required_samples <= capacity,
            "{name}: two cycles at {lowest_hz} Hz need {required_samples} samples, but the ring \
             sized itself to {capacity}"
        );

        // Every row must run long enough for the optical summary's required
        // three phase markers (two complete cycles), not only the lowest one.
        for (index, point) in protocol.points.iter().enumerate() {
            let recorded_cycles = point.frequency_hz * point.duration_s as f64;
            assert!(
                recorded_cycles >= 2.0,
                "{name} point {} records only {recorded_cycles:.3} cycles",
                index + 1
            );
        }

        // The regression witness: without the marker period, the same default
        // cache is far too short for these files. That gap is what used to be
        // an operator precondition, and what a whole survey failed on.
        if lowest_hz < 0.1 {
            let unmarked = SharedState {
                cache_seconds: DEFAULT_CACHE_SECONDS,
                ..SharedState::default()
            };
            assert!(
                required_samples > unmarked.ring_capacity(BENCH_RATE_HZ),
                "{name}: witness no longer proves the 20 s default is insufficient on its own"
            );
        }
    }
}
