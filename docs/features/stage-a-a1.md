# Stage-A A1 Analysis

- **Crate:** `plugins/stage-a-a1` (`augur-plugin-stage-a-a1`)
- **Status:** Recording coordinator + live quicklooks + amplitude sweep + `a₀` lock
  + unattended frequency ladder
- **Design:** [ADR 009](../adr/009-stage-a-a1-recording-coordinator.md),
  [ADR 010](../adr/010-stage-a-a1-amplitude-sweep.md) (sweep + button
  press forwarding),
  [ADR 015](../adr/015-stage-a-a1-recording-robustness.md) (one folder, full
  duration, named failures),
  [ADR 014](../adr/014-stage-a-a1-frequency-ladder.md) (the unattended ladder),
  [ADR 013](../adr/013-stage-a-a1-event-count-depth-lock.md) (exact-event-count
  `a₀` lock)
- **Automation roadmap:** [Stage-A A1 Automation](./stage-a-a1-automation.md)
- **Second workflow:** [Stage-A A1 Exact Event Count](./stage-a-a1-event-count.md)
  — hold one *measured* depth `a₀` across the frequency sweep

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
| Sweep min a / max a | the `a`-range for this row; the **Start sweep** button records it, and it is stored in every sidecar |
| Sweep points (count) | how many amplitudes Start sweep records, spaced evenly over `[min a, max a]` |
| Sweep settle (s) | dwell the photodiode-measured `a` must hold the target (±10 %, ≥±0.05) before each sweep recording; 30 s cap, then it records anyway |
| Duration (s) | each recording auto-stops and finalizes after this |
| Start recording (sweep point) | start camera RAW → connect and lease photodiode → start PDQ → auto-stop and save both → sidecar |
| Start sweep (record all points) | per point: lease the modulation owner → retarget the calibrated drive to `a_i` → settle → one recording (`…_pNN`) → next point |
| Record pilot | records a bright reference (`…_pilot`) **and** freezes the ON/OFF windows for the row from the live signal |
| Record background | records an unmodulated reference (`…_background`) **and** captures the false-response floor `q0` |
| Stop (abort recording / sweep) | finalize the current recording early; during a sweep also aborts the remaining points |
| a₀ / Find a₀ / Record a₀ point / Start frequency sweep | the **exact-event-count** workflow: hold one *measured* depth `a₀` across the frequency sweep, by hand or as an unattended ladder — see [its brief](./stage-a-a1-event-count.md) |

The record and sweep buttons stay **disabled until an output folder is
selected**.

For manual recordings A1 never drives the Teensy: set the drive (high `a` for
the pilot, `a≈0` for the background) in the modulation plugin, then press the
matching button — the recording captures whatever `a` is currently set.

**The sweep is the one scoped exception.** Start sweep leases the modulation
owner (`SERVICE_STAGE_A_MODULATION_CONTROL_V1`) and, per point, issues
`ModulationCommandV1::SetOpticalDepth` — which only retargets the *depth* of the
drive the operator already armed (waveform, frequency, operating point `I_k`,
and calibration stay untouched; the owner refuses when a manual-DAC or constant
drive is armed). It renews the lease per point, waits for the
photodiode-measured `a` to settle, hands the point to the normal recording
coordinator, and releases the lease at the end or on abort. Sweep points
require `min a > 0` — record `a≈0` with the background button instead. Sidecars
of sweep recordings additionally carry `sweep.requested_a`, `sweep.point_index`
and `sweep.point_total`. After the sweep releases the lease, the drive holds the
last sweep amplitude until the operator's own `depth a` setting is re-applied
(any modulation settings change re-sends it) — which is exactly why an
event-count point re-applies its locked depth under the lease instead of trusting
the drive to still be where a previous action left it (ADR 013).

**Naming.** Files share an `<id>_<timestamp>[_role]` stem under an `<id>/` subfolder
(`_pilot` / `_background` tag the reference runs, `_ec_f<f>Hz` an event-count point):

- `<id>/<id>_<ts>.raw` — camera RAW, with the host's own `<stem>.toml` sidecar
  (camera biases, ROI) next to it.
- `<id>/<id>_<ts>_pd.pdq` + `_pd.json` — photodiode PDQ + sidecar.
- `<id>/<id>_<ts>_config.toml` — the A1 sidecar.

**Everything lands under `<A1 output folder>/<id>/`** (ADR 015). That folder is
the only setting deciding where a measurement ends up — the host output root and
the photodiode Data directory no longer have to be kept aligned by hand:

- **The PDQ and its sidecar are written there directly.** A1 names the
  destination root in the start spec (`PdqStartSpecV1::root_dir`), which replaces
  the photodiode's own Data directory for that run. An A1-driven recording
  therefore does not depend on the photodiode's folder setting at all.
- **The camera RAW and the host's bias `.toml` are moved there after
  finalization.** The host resolves plugin recording paths below *its* output
  directory and rejects absolute ones, so A1 cannot name the destination up
  front; instead it gathers the file once the host reports it closed and hashed.
  A rename on one volume, a size-verified copy across volumes. A file that cannot
  be moved stays where it is and the sidecar points at it there.

**A1 config sidecar** captures: `measurement_id`, file stem, role, start/finalize
timestamps, duration; the sweep `[min_a, max_a]`; modulation settings from the
acknowledged snapshot (frequency, center/amplitude DAC, waveform); the
photodiode-measured `a` (`measured_log_contrast`) and clip fractions; ROI +
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

**When something is wrong** (ADR 015):

- **Before the camera starts**, A1 refuses the recording — writing nothing — if
  the photodiode is not reporting status, is not connected, or is leased by
  someone else. The same hint fills the status `message` cell while idle, so it
  is visible before the button is pressed. (The photodiode's *Data directory* is
  deliberately not among these: A1 supplies the destination itself.)
- **If the photodiode fails once the camera is running**, the camera keeps
  recording for the full requested duration and closes normally. The run is
  marked camera-only: `recording_completed_ok` stays false (so a sweep stops),
  but the RAW is complete rather than a truncated stub.
- **The first, most specific failure is what you see.** The closing message is
  `Recording <id> incomplete: <cause> — metadata saved to <path>`; later fallout
  cannot overwrite the original cause.
- **Starting and stopping the host recorder restarts the capture pipeline**, which
  the host reports as a `SourceChanged` discontinuity — twice per recording. While
  a recording or sweep is in flight that boundary resets only the event fold, not
  the row's pilot windows, background floor, or collected response points.

**Host-side note.** The camera RAW leg restarts the host pipeline into
Recording mode and stops it again at finalize. After the file is finalized, the
host restores Preview before returning the receipt, so a sweep or another button
press can start the next recording automatically.

**File locations.** One place: `<A1 output folder>/<id>/` holds `<stem>.raw`
(+ the host's `<stem>.toml`), `<stem>_pd.pdq` + `<stem>_pd.json`, and
`<stem>_config.toml`.

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
background floor survive ordinary UI interaction. Source changes and seeks reset
everything **unless** a recording or sweep is in flight, in which case the
boundary is A1's own pipeline restart and only the event fold resets (ADR 015).

## Where the inputs come from

| Input | Source |
|---|---|
| camera events, valid pixels | retained **EventStore** over a trailing analysis window; falls back to `frame.events()`, trimmed to the same window |
| phase-0 markers | rising `frame.external_triggers()` — the host **banks trigger edges from dropped preview frames** into the next processed frame (drain-to-newest and the preview throttle drop whole frames; at low modulation frequencies the survivors alone rarely held 2 markers inside the analysis window) |
| modulation period `T` | measured from the `EXT_TRIGGER` marker spacing; else the modulation plugin's acknowledged waveform — which, since the board-echo fallback, includes the **operator-armed UI drive**, not only service-path (leased) targets |
| optical modulation depth `a` | photodiode plugin's optical summary (`measured_log_contrast`) — always the *excitation* contrast, independent of that plugin's display mode (ADR 012) |
| ROI, masked pixels | augur-rs camera config (`CTX_GLOBAL_SETTINGS`) |

## Tests

`cargo test -p augur-plugin-stage-a-a1` covers trigger-defined period, marker-anchored
folding, ON/OFF separation of the rolling dataset, auto-window detection and the `q_p`
path, file-safe id generation, UTC timestamp formatting, the config-sidecar builder,
the pilot-window round-trip through the measurement folder, press-latch edge/baseline
semantics, the jittery-marker free-running fallback, sweep-point spacing, the
sweep-point sidecar fields, the ordered camera → PDQ → PDQ finalize → camera
finalize lifecycle (including envelope identity/revision and save location), the
selective discontinuity reset, and the `a₀`-lock and frequency-ladder sets listed
in the [exact-event-count brief](./stage-a-a1-event-count.md).

Three of them guard the recording defects fixed in ADR 015: a photodiode leg that
cannot start is refused before any host command is sent; a photodiode failure
mid-run keeps the camera recording for the full duration, names the cause in the
closing message, and still gathers the RAW and its bias sidecar into the
measurement folder; and a self-inflicted `SourceChanged` during a recording keeps
the row's response points and pilot windows while still resetting the event fold.
