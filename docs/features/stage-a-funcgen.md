# Stage-A Function Generator (`stage-a-funcgen`)

> Feature brief — familiarisation plugin for the Stage-A bench.
> Protocol source of truth:
> `stage-a-controller/docs/features/waveform-drive.md` (reserved v2 fields).

## Purpose

Manual Pockels-cell drive control for getting to know the setup: waveform
(sine / square / sawtooth), frequency, and DAC modulation depth, with the
resulting optical amplitude always **measured** from the photodiode as
`a = ln(V_max/V_min)` — the Pockels V→T response is non-linear, so the DAC
excursion never doubles as a light level.

## Included

- `plugins/stage-a-funcgen` crate (`augur-plugin-stage-a-funcgen`): connect /
  apply / stop actions, live photodiode waveform view, status table with the
  measured contrast, clipping, and stream integrity;
- **`mock` port** (default): the waveform-extended mock controller runs on an
  in-process thread and streams a synthetic photodiode response through a
  Pockels-like sin² transfer — the complete control loop with zero hardware;
- feature detection against real firmware: 0.2.0 rejects the reserved drive
  fields with `unknown_config_field`, which the plugin reports as "no
  waveform backend" instead of a fault;
- same fail-closed safety model as `stage-a-monitor` (`LiveCapture` +
  `effects_allowed` only; drive parameters are settings, applying them is an
  explicit action; local bounds check before any command is sent).

## Firmware-faithful mock (stage-a-io)

Delivered together with this plugin, `stage-a-io`'s `MockController` now
mirrors firmware 0.2.0 exactly — verbs, state machine (`SAFE_IDLE` →
`CONFIGURED` → `RUNNING`), error codes/details, single-entry idempotent reply
cache, and unknown-CONFIG-field rejection. The previous mock accepted verbs
and fields the device does not speak (`ARM`, `RUN`, `capabilities=`,
`BAD_*`), which let host bugs pass tests: the A1 sweep reconfigured while
RUNNING (now fixed with STOP-before-CONFIG) and watchdog `!FAULT` notices
were invisible outside an in-flight request (now surfaced as async events by
`StageAClient` and handled by all three plugins).

## Verification

`cargo test -p stage-a-io -p augur-plugin-stage-a-funcgen
-p augur-plugin-stage-a-monitor -p augur-plugin-stage-a-a1`: mock
state-machine/error fidelity against `main.cpp`, v1 rejection of waveform
fields, drive-bounds validation, nonlinear sin² contrast response, watchdog
fault propagation, and the full mock round trip (connect → apply sine /
square / saw → measured `a` → stop → reconfigure while driving).

## Known gaps

- Real firmware cannot emit a waveform yet; the `waveform-drive.md` fields
  stay host+mock-only until the hardware freeze resolves the DAC channel and
  safe HVA window.
- No PDQ/sidecar recording in this plugin — it is a familiarisation tool;
  evidence-grade recording stays with `stage-a-monitor`/`stage-a-a1`.
