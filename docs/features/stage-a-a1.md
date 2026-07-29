# Stage-A A1 Analysis

- **Crate:** `plugins/stage-a-a1` (`augur-plugin-stage-a-a1`)
- **Status:** Recording coordinator + live quicklooks + amplitude sweep
- **Design:** [ADR 009](../adr/009-stage-a-a1-recording-coordinator.md),
  [ADR 010](../adr/010-stage-a-a1-amplitude-sweep.md) (sweep + button
  press forwarding)
- **Automation roadmap:** [Stage-A A1 Automation](./stage-a-a1-automation.md)

## Purpose

A1 has two jobs on the Stage-A bench, both deliberately thin:

1. **Recording coordinator.** One *Start recording* button records the camera
   **RAW** stream and the photodiode **PDQ** stream together for a fixed duration,
   grouped under a per-`(I_k, f)` measurement id, and writes an A1 **config
   sidecar** (`.toml`) linking them with everything needed to reproduce and
   analyse the run offline.
2. **Live sanity quicklooks.** The rolling half-period response `S_p(t)` and the
   response probability `q_p`, folded on the modulation period `T`.

A1 owns no hardware and never drives the Teensy. The optical drive is armed in the
modulation plugin; A1 only *reads* its published settings.

## The recording workflow

The experiment sweeps the modulation depth `a = ln(I_max/I_min)` at a fixed
illumination `I_k` and frequency `f`, taking several recordings per `(I_k, f)`
pair (a background `a≈0`, a bright pilot, then settled amplitudes). One
**measurement id = one `(I_k, f)` row**; every recording under it lands in the same
folder. A1 makes each recording one button press:

| Control | Meaning |
|---|---|
| Output folder | where the A1 config sidecar is written (recommended shared experiment root) |
| Measurement id | one per `(I_k, f)` row; auto-generated default, editable, or press **New id** |
| Physical `I_k` flux point id | required canonical id of the cycle-mean local flux calibration/map point; never inferred from the modulator's normalized mean `ū` |
| Sweep min a / max a | the `a`-range for this row; the **Start sweep** button records it, and it is stored in every sidecar |
| Sweep points (count) | how many amplitudes Start sweep records, spaced evenly over `[min a, max a]` |
| Sweep settle (s) | dwell the fresh photodiode-measured `a` must hold the target (±10 %, ≥±0.05) before each sweep recording; timeout aborts the sweep |
| Duration (s) | each recording auto-stops and finalizes after this |
| Start recording (sweep point) | start camera RAW → connect and lease photodiode → start PDQ → auto-stop and save both → sidecar |
| Start sweep (record all points) | per point: lease the modulation owner → retarget the calibrated drive to `a_i` → settle → one recording (`…_pNN`) → next point |
| Record pilot | records a bright reference (`…_pilot`) **and** freezes the ON/OFF windows for the row from the live signal |
| Record background | records an unmodulated reference (`…_background`) **and** captures the false-response floor `q0` |
| Stop (abort recording / sweep) | finalize the current recording early; during a sweep also aborts the remaining points |

The record and sweep buttons stay **disabled until an output folder is
selected**.

For manual recordings A1 never drives the Teensy: set the drive (high `a` for
the pilot, `a≈0` for the background) in the modulation plugin, then press the
matching button — the recording captures whatever `a` is currently set.

**The sweep is the one scoped exception.** Start sweep leases the modulation
owner (`SERVICE_STAGE_A_MODULATION_CONTROL_V1`) and, per point, issues
`ModulationCommandV1::SetOpticalDepth` — which only retargets the *depth* of the
drive the operator already armed (frequency, normalized cycle mean `ū`, and
calibration stay untouched). The modulation owner accepts this command only
with an applied measured calibration and `OPTICAL_LOG_SINE`; manual, constant,
DAC-sine, square, and optical-linear modes are rejected. It renews the lease per
point, waits for a fresh, marker-bounded photodiode `a` from a confirmed `I_tot`
anchor to settle, hands the point to the normal recording
coordinator, and releases the lease at the end or on abort. Sweep points
require `min a > 0` — record `a≈0` with the background button instead. Sidecars
of sweep recordings additionally carry `sweep.requested_a`, `sweep.point_index`
and `sweep.point_total`. After the sweep releases the lease, the drive holds the
last sweep amplitude until the operator's own `depth a` setting is re-applied
(any modulation settings change re-sends it).

**Naming.** Files share an `<id>_<timestamp>[_role]` stem under an `<id>/` subfolder
(`_pilot` / `_background` tag the reference runs):

- `<id>/<id>_<ts>.raw` — camera RAW, under the **host output root**, with the host's
  own `<stem>.toml` sidecar (camera biases, ROI) written next to it.
- `<id>/<id>_<ts>_pd.pdq` + `_pd.json` — photodiode PDQ + sidecar, under the
  **photodiode data root**.
- `<id>/<id>_<ts>_config.toml` — the A1 sidecar, under the chosen output folder.

Each recorder confines its writes to its own root, so A1 cannot force one absolute
directory (see ADR 009). Point the host output root and the photodiode data root
at the same experiment directory to co-locate everything; the A1 sidecar records
the *resolved* paths so the set stays linked either way.

**A1 config sidecar** captures: `measurement_id`, physical `flux_point_id`, file
stem, role, start/finalize
timestamps, duration; the sweep `[min_a, max_a]`; modulation settings from the
acknowledged snapshot (frequency, center/amplitude DAC, waveform, transfer
`calibration_id`, optical target, requested and resolved normalized mean `ū`,
internal `u_g`/`u_c`, requested `a`, `V_null` and `Vπ`); the
photodiode-measured `a`, extrema, geometric pedestal,
headroom, clip fractions, dark/ADC ids, and named dark-corrected `I_tot` anchor; ROI +
masked-pixel count + `N_valid`; trigger info (marker-anchored, marker count,
measured period); and the resolved paths of the RAW (+ its camera-config sidecar)
and the PDQ (+ its sidecar). The **pilot** run additionally records the frozen
ON/OFF windows and the **background** run the floor `q0`, so returning to a
measurement (folder + id) auto-reloads them for the `q_p` plot.

**Mechanism.** A small control-plane state machine in `process_control` starts
the host camera recorder first and waits for its receipt. Only after the host
has completed the Preview → Recording switch does A1 connect and lease the
photodiode and open the PDQ with the same run id. The duration begins when the
PDQ start receipt arrives, so setup time is never deducted from the requested
recording. On completion A1 atomically finalizes the PDQ and releases its lease
while camera effects are still live, then stops the host recorder, waits for its
final receipt, and writes the config sidecar. A recording is successful only
when the host receipt is complete and the photodiode returns a valid finalized
receipt with both PDQ paths. The status panel shows only the current phase and
one concise result or error message; it does not render an internal event log.
A1 declares `host_commands = ["start_recording", "stop_recording"]` in its
manifest. Every role uses this same lifecycle.

**Host-side note.** The camera RAW leg restarts the host pipeline into
Recording mode and stops it again at finalize. After the file is finalized, the
host restores Preview before returning the receipt, so a sweep or another button
press can start the next recording automatically.

**File locations** (three roots, point them at the same experiment directory):
`<host output root>/<id>/<stem>.raw` (+ host `<stem>.toml`),
`<photodiode data dir>/<id>/<stem>_pd.pdq` + `<stem>_pd.json`, and
`<A1 output folder>/<id>/<stem>_config.toml`.

## The two live plots

Both fold the camera event stream on `T` (from the firmware phase-0 `EXT_TRIGGER`
marker spacing, which *defines* the frequency; the modulation acknowledged waveform
is the only fallback). Enable **Live analysis** to keep them updating.

Marker hygiene: preview windows overlap, so the same trigger edge arrives on
several consecutive frames — the marker buffer is sorted and deduplicated on
every merge (duplicates used to fail marker validation and blank the plots).
When marker validation still rejects a fold (dropped-trigger jitter), the
quicklook falls back to the free-running fold on `T` instead of going empty.

1. **Rolling half-period response**

   ```math
   S_p(t) = \frac{N_p(t-T/2,\,t]}{N_\text{valid}}
   ```

   events per valid pixel in the trailing half-cycle, ON and OFF. A live indicator:
   are events appearing, does the ON/OFF timing look sane, is the response
   saturating? It counts *every* event in the ROI, so a noisy pixel weighs heavily
   — it is a quicklook, not the response metric.

   `N_valid` is **ROI area minus masked pixels**, the same denominator `q_p` uses,
   and the numerator counts only events inside that same region. The two are shown
   side by side and have to mean the same thing; normalising `S_p` over the whole
   sensor under-reported it by the ROI/frame ratio while counting events from
   outside the ROI.

2. **Response probability** `q_p`

   ```math
   z_{i,c,p} = \mathbf{1}[\text{pixel } i \text{ fires in } W_p \text{ during cycle } c],
   \qquad
   \hat q_p(a,f) = \frac{1}{N_\text{valid} M}\sum_i\sum_c z_{i,c,p}
   ```

   the fraction of valid pixel-cycles that fire at least once in the ON/OFF phase
   window `W_p` — each pixel-cycle counts **once** (unlike `S_p`). The windows come
   from the row's **pilot** when one has been recorded (frozen, held across the
   whole row), otherwise from the trigger-anchored fold automatically: since the
   `EXT_TRIGGER` fixes the phase, ON and OFF live in opposite half-cycles, so each
   window is anchored on its histogram peak and grown outward until events fall
   below the **window floor** (default 10 % of the peak) or the opposite polarity
   takes over. `Record point` appends one `(measured a, q_on, q_off)` dot. The ROI
   and masked pixels come from the augur-rs camera config
   (`N_valid = |ROI| − |masked|`).

   **Why the pilot is per row.** The window phase depends on the event latency,
   which is a *phase* shift `τ·f` — negligible at low `f`, up to a full cycle at
   high `f` — and also drifts with `I_k`. So the windows must be defined **per
   `(I_k, f)` row** and held fixed across that row's `a`-sweep (re-deriving them
   per amplitude would bias the curve). One pilot per measurement id captures that
   exactly. This live `q_p` stays a quicklook; the **authoritative** `q_p(a, f)`
   fit (`a50`, background floor) is computed offline from the recordings.

## Button presses across the UI-mirror / live-worker split

The host loads two instances of every dynamic plugin: a **UI mirror** (renders
the settings, never touches hardware) and the **live worker** (runs
`process_frame` / `process_control`, owns the recording state machine). A
`SettingKind::Button` click calls `set_setting(key, true)` **on the mirror
only**; the worker receives settings through the host's snapshot, which carries
whatever `get_setting` returns. A1 therefore exports every button as a
**monotonic press counter** (`PressLatch`): the mirror increments it per click,
the snapshot transports it, and the worker treats a counter advance as exactly
one press edge (the first value a freshly loaded worker sees is adopted
silently, so reloads never replay old presses). This is why the record buttons
used to do nothing — the presses died on the mirror.

Related: A1 overrides `on_discontinuity` to ignore `SettingsChanged` (raised on
*every* settings sync of any plugin), so the response curve, pilot windows and
background floor survive ordinary UI interaction; source changes and seeks
still reset everything.

## Where the inputs come from

| Input | Source |
|---|---|
| camera events, valid pixels | retained **EventStore** over a trailing analysis window; falls back to `frame.events()`, trimmed to the same window |
| phase-0 markers | rising `frame.external_triggers()` — the host **banks trigger edges from dropped preview frames** into the next processed frame (drain-to-newest and the preview throttle drop whole frames; at low modulation frequencies the survivors alone rarely held 2 markers inside the analysis window) |
| modulation period `T` | measured from the `EXT_TRIGGER` marker spacing; else the modulation plugin's acknowledged waveform — which, since the board-echo fallback, includes the **operator-armed UI drive**, not only service-path (leased) targets |
| optical modulation depth `a` | fresh photodiode optical summary (`measured_log_contrast`) from complete marker-bounded cycles and a confirmed `I_tot` anchor — always the *excitation* contrast, independent of display mode (ADR 012) |
| ROI, masked pixels | augur-rs camera config (`CTX_GLOBAL_SETTINGS`) |

## Tests

`cargo test -p augur-plugin-stage-a-a1` covers trigger-defined period, marker-anchored
folding, ON/OFF separation of the rolling dataset, auto-window detection and the `q_p`
path, file-safe id generation, UTC timestamp formatting, the config-sidecar builder,
the pilot-window round-trip through the measurement folder, press-latch edge/baseline
semantics, the jittery-marker free-running fallback, sweep-point spacing, the
sweep-point sidecar fields, the ordered camera → PDQ → PDQ finalize → camera
finalize lifecycle (including envelope identity/revision and save location), and
the selective discontinuity reset.
