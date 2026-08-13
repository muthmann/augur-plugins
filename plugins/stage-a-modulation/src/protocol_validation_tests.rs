//! End-to-end static validation of the A1 bench protocols against the same
//! coupled optical-drive calculations the modulation owner uses at runtime.

use std::path::Path;

use augur_plugin_api::{
    ExecutionContext, ExecutionMode, Plugin, PluginRuntimeRole, PluginServiceOutcome,
    PluginServiceRequest,
};
use augur_plugin_stage_a_a1::protocol::{parse_csv, ProtocolPoint};
use serde_json::Value;
use stage_a_plugin_contract::{
    ClientId, LeaseId, ModulationCommandV1, ModulationRequestV1, RunId,
    PLUGIN_ID_STAGE_A_MODULATION, SERVICE_STAGE_A_MODULATION_CONTROL_V1,
};

use super::waveform::{
    log_sine_geometric_pedestal, LobeInversion, OpticalDrive, OpticalTarget, PeakLaw,
    DAC_FULL_SCALE, DEPTH_A_MAX, DEPTH_A_MIN, MEAN_U_MIN,
};
use super::{now_unix_ms, DriveMethod, Mode, StageAModulationPlugin};

// A conservative policy of the qualified A1 protocol set, in addition to the
// modulation owner's measured-lobe and DAC ceilings.
const PEAK_U_GUARD: f64 = 0.90;

struct Fixture {
    name: &'static str,
    csv: &'static str,
    expected_points: usize,
}

fn fixtures() -> [Fixture; 5] {
    [
        Fixture {
            name: "a1_triage_90min.csv",
            csv: include_str!("../../stage-a-a1/protocols/a1_triage_90min.csv"),
            expected_points: 40,
        },
        Fixture {
            name: "a1_stufe1_bode_dc.csv",
            csv: include_str!("../../stage-a-a1/protocols/a1_stufe1_bode_dc.csv"),
            expected_points: 73,
        },
        Fixture {
            name: "a1_stufe2_bode_u010.csv",
            csv: include_str!("../../stage-a-a1/protocols/a1_stufe2_bode_u010.csv"),
            expected_points: 47,
        },
        Fixture {
            name: "a1_stufe2_bode_u045.csv",
            csv: include_str!("../../stage-a-a1/protocols/a1_stufe2_bode_u045.csv"),
            expected_points: 47,
        },
        Fixture {
            name: "a1_stufe2_flussleiter.csv",
            csv: include_str!("../../stage-a-a1/protocols/a1_stufe2_flussleiter.csv"),
            expected_points: 231,
        },
    ]
}

/// The applied bench calibration and DAC ceiling used to qualify the files.
/// Endpoint rounding mirrors `apply_calibration`; resolving the pair mirrors
/// the modulation owner's `lobe_inversion` path.
fn qualified_lobe() -> (LobeInversion, f64) {
    let calibration: Value =
        serde_json::from_str(include_str!("../testdata/pockels-20260730-083123.json"))
            .expect("recorded Pockels calibration JSON");
    let number = |key: &str| {
        calibration[key]
            .as_f64()
            .unwrap_or_else(|| panic!("calibration has no numeric {key}"))
    };
    let v_null = number("v_null_dac");
    let v_peak = v_null + number("v_pi_dac");
    let inversion =
        LobeInversion::resolve(v_null.round(), v_peak.round(), f64::from(DAC_FULL_SCALE))
            .expect("recorded calibration resolves to a drivable lobe")
            .inversion;
    (inversion, number("max_level"))
}

fn milli(value: f64) -> f64 {
    (value * 1_000.0).round() / 1_000.0
}

fn live_execution() -> ExecutionContext {
    ExecutionContext {
        mode: ExecutionMode::LiveCapture,
        effects_allowed: true,
        session_id: Some("a1-protocol-validation".into()),
    }
}

fn service_request(
    plugin: &StageAModulationPlugin,
    id: u64,
    command: ModulationCommandV1,
) -> PluginServiceRequest {
    let mut payload = ModulationRequestV1::new(
        stage_a_plugin_contract::RequestId(id),
        ClientId::from("stage-a.a1"),
        command,
    );
    payload.target_owner_instance = Some(plugin.owner_instance.clone());
    payload.run_id = Some(RunId::from("a1-protocol-validation"));
    payload.lease_id = Some(LeaseId::from("a1-protocol-validation"));
    payload.issued_at_unix_ms = now_unix_ms();
    PluginServiceRequest {
        request_id: id,
        source_plugin_id: "stage-a.a1".into(),
        target_plugin_id: PLUGIN_ID_STAGE_A_MODULATION.into(),
        service: SERVICE_STAGE_A_MODULATION_CONTROL_V1.into(),
        payload: serde_json::to_value(payload).expect("serializing modulation request"),
    }
}

fn qualified_service_owner() -> StageAModulationPlugin {
    let calibration: Value =
        serde_json::from_str(include_str!("../testdata/pockels-20260730-083123.json"))
            .expect("recorded Pockels calibration JSON");
    let number = |key: &str| {
        calibration[key]
            .as_f64()
            .unwrap_or_else(|| panic!("calibration has no numeric {key}"))
    };

    let mut plugin = StageAModulationPlugin::default();
    plugin.runtime_role = PluginRuntimeRole::LiveWorker;
    plugin.effects_allowed = true;
    plugin.port_hint = "mock".into();
    plugin.connect_requested = true;
    plugin.max_level = number("max_level").round() as i64;
    plugin.method = DriveMethod::Calibrated;
    plugin.mode = Mode::OpticalLogSine;
    plugin.frequency_hz = 0.10;
    plugin.depth_a = 1.70;
    plugin.operating_point = 0.30;
    plugin.v_null_dac = number("v_null_dac").round() as i64;
    plugin.v_peak_dac = (number("v_null_dac") + number("v_pi_dac")).round() as i64;
    plugin.calibration_id = Some("pockels-20260730-083123".into());
    plugin.connect();
    assert!(
        plugin.link.is_some(),
        "mock modulation owner did not connect"
    );
    plugin
}

fn assert_service_accepts(
    plugin: &mut StageAModulationPlugin,
    request_id: u64,
    command: ModulationCommandV1,
    context: &str,
) {
    let request = service_request(plugin, request_id, command);
    let reply = plugin.handle_service_request(&request, &live_execution());
    assert!(
        matches!(reply.outcome, PluginServiceOutcome::Accepted { .. }),
        "{context}: production modulation service rejected the request: {:?}",
        reply.outcome
    );
}

/// Rebuilds the exact optical-log-sine command the owner would send after the
/// A1 service has rounded all three protocol coordinates to milli-units.
fn assert_drive_is_accepted(
    fixture: &str,
    point: usize,
    mean_u: f64,
    frequency_hz: f64,
    depth_a: f64,
    inversion: LobeInversion,
    max_code: f64,
) {
    let mean_u = milli(mean_u);
    let frequency_millihz = (frequency_hz * 1_000.0).round() as u64;
    let frequency_hz = frequency_millihz as f64 / 1_000.0;
    let depth_a = milli(depth_a);
    let context = format!(
        "{fixture} point {}: ū={mean_u:.3}, f={frequency_hz:.3}, a={depth_a:.3}",
        point + 1
    );

    assert!(
        (MEAN_U_MIN..=1.0).contains(&mean_u),
        "{context}: mean_u is outside the modulation service range"
    );
    assert!(
        (DEPTH_A_MIN..=DEPTH_A_MAX).contains(&depth_a),
        "{context}: depth_a is outside the modulation service range"
    );
    assert!(
        stage_a_plugin_contract::drive_frequency_supported(frequency_millihz),
        "{context}: frequency is outside the plugin/firmware range"
    );

    let law = PeakLaw::LogSine;
    let u_max = inversion.peak_intensity_ceiling(max_code);
    let peak_u = law.peak(mean_u, depth_a);
    assert!(
        depth_a <= law.max_depth_for_mean(mean_u, u_max) + 1e-9,
        "{context}: a exceeds the coupled max-depth calculation"
    );
    assert!(
        mean_u <= law.max_mean_for_depth(depth_a, u_max) + 1e-9,
        "{context}: mean_u exceeds the coupled max-mean calculation"
    );
    assert!(
        peak_u <= PEAK_U_GUARD + 1e-9,
        "{context}: peak u={peak_u:.6} exceeds the protocol guard {PEAK_U_GUARD:.2}"
    );

    // The service publishes the Bessel-normalized geometric pedestal in
    // milli-units. Test the rounded table, not an ideal higher-precision one.
    let pedestal_u = milli(log_sine_geometric_pedestal(mean_u, depth_a));
    let table = OpticalDrive {
        target: OpticalTarget::LogSine,
        depth_a,
        operating_point: pedestal_u,
        inversion,
    }
    .warp_table()
    .unwrap_or_else(|error| panic!("{context}: firmware warp would be refused: {error}"));
    let highest_code = table.iter().copied().max().unwrap_or(0);
    assert!(
        f64::from(highest_code) <= max_code,
        "{context}: warp needs DAC {highest_code}, above configured max {max_code:.0}"
    );
}

/// The protocol sends mean, frequency and depth as three ordered service
/// requests. Validate the intermediate states too: a valid final `(ū, a)` is
/// not enough if changing `ū` first would be rejected against the previous a.
fn assert_protocol_transitions_are_accepted(
    fixture: &str,
    points: &[ProtocolPoint],
    inversion: LobeInversion,
    max_code: f64,
) {
    // Required pre-flight state in the protocol comments/UI: the armed depth
    // must not exceed the largest depth the file will request.
    let (mut frequency_hz, mut depth_a) = (0.10, 1.70);
    for (index, point) in points.iter().enumerate() {
        assert_drive_is_accepted(
            fixture,
            index,
            point.mean_u,
            frequency_hz,
            depth_a,
            inversion,
            max_code,
        );
        let mean_u = point.mean_u;
        assert_drive_is_accepted(
            fixture,
            index,
            mean_u,
            point.frequency_hz,
            depth_a,
            inversion,
            max_code,
        );
        frequency_hz = point.frequency_hz;
        assert_drive_is_accepted(
            fixture,
            index,
            mean_u,
            frequency_hz,
            point.depth_a,
            inversion,
            max_code,
        );
        depth_a = point.depth_a;
    }
}

/// Runs the parsed rows through the real modulation service boundary. This is
/// deliberately in addition to the named policy assertions above: a change in
/// lease, mode, lobe, rounding, `drive_command` or service sequencing must make
/// the laboratory fixtures fail here rather than drift from production.
fn assert_production_service_accepts_protocol(fixture: &str, points: &[ProtocolPoint]) {
    let mut plugin = qualified_service_owner();
    let mut request_id = 1;
    assert_service_accepts(
        &mut plugin,
        request_id,
        ModulationCommandV1::AcquireLease { ttl_ms: 60_000 },
        fixture,
    );

    for (index, point) in points.iter().enumerate() {
        let context = format!("{fixture} point {}", index + 1);
        for command in [
            ModulationCommandV1::SetOperatingPoint {
                mean_u_milli: (point.mean_u * 1_000.0).round() as u32,
            },
            ModulationCommandV1::SetDriveFrequency {
                frequency_millihz: (point.frequency_hz * 1_000.0).round() as u64,
            },
            ModulationCommandV1::SetOpticalDepth {
                depth_a_milli: (point.depth_a * 1_000.0).round() as u32,
            },
        ] {
            request_id += 1;
            assert_service_accepts(&mut plugin, request_id, command, &context);
        }
    }
    plugin.disconnect();
}

#[test]
fn shipped_a1_protocols_parse_and_every_retarget_is_drivable() {
    let (inversion, max_code) = qualified_lobe();
    for fixture in fixtures() {
        let protocol = parse_csv(fixture.csv)
            .unwrap_or_else(|error| panic!("{} does not parse: {error}", fixture.name));
        assert_eq!(
            protocol.points.len(),
            fixture.expected_points,
            "{} changed recording count",
            fixture.name
        );
        assert_protocol_transitions_are_accepted(
            fixture.name,
            &protocol.points,
            inversion,
            max_code,
        );
        assert_production_service_accepts_protocol(fixture.name, &protocol.points);

        // On the bench these files live in Playground. When that sibling tree
        // exists, make drift from the versioned, shipped fixture a test failure.
        let live = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../Playground/protocols")
            .join(fixture.name);
        if live.is_file() {
            let live_text = std::fs::read_to_string(&live)
                .unwrap_or_else(|error| panic!("cannot read {}: {error}", live.display()));
            assert_eq!(
                live_text,
                fixture.csv,
                "{} differs from the protocol shipped and validated by the plugin",
                live.display()
            );
        }
    }
}
