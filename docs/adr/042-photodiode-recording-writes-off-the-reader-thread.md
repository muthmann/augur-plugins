# ADR 042 — The photodiode recording is written off the reader thread

- Status: accepted
- Date: 2026-09-07
- Supersedes: nothing. Affects the recording path behind
  [ADR 007](./007-stage-a-owner-orchestration.md) evidence files, and therefore
  every A1/A2 point ([ADR 038](./038-stage-a-a2-protocol-runner.md),
  [ADR 041](./041-stage-a-a2-drive-sync-capture.md)).

## Context

The photodiode owner reads the free-running PDA1 stream on one thread. That
thread also wrote the `.pdq` evidence file: `record_frame` called
`PdqWriter::write_frame` inline, through an 8 KiB `BufWriter`, so roughly every
second wire frame turned into one small write to the storage target — which at
the bench is a network share.

The firmware keeps **two** DMA blocks of 2048 samples. At the bench rate of
500 kSa/s that is about 8 ms of slack: a block whose frame cannot be staged
while the previous one still drains is discarded whole and counted
(`stream_dropped_samples`). Every millisecond the reader spends inside a write
is a millisecond in which the device can overrun.

That is what a stalled write costs, measured on the retained evidence of
2026-09-07 (`A2-…_core_floor_pre`, two consecutive attempts):

- The device dropped 176 128 and 178 176 samples — 2 to 4 losses of 12 to
  283 ms each — while `crc_failures`, `resync_bytes` and the frame sequence
  stayed clean. Nothing was lost on the wire; the device threw the blocks away.
- The firmware drop counter did **not** move in the ~100 s between the two
  recordings. The losses happened only while a recording was open.
- A `.pdq` with a hole is not one contiguous segment, so
  `FrameTracker::contiguous_sample_range` yields `None`, the finalized receipt
  carries no `sample_range`, and A2 refuses the point with *"PDQ does not cover
  the requested 30.000 s: sampled duration=None s"* — after the full 30 s ran.
  The A2 identification points need 100 s contiguous, so this is not a
  wait-and-retry situation.

The same host wrote 30 clean 20 s recordings to the same share on 2026-08-14,
so this is storage latency, not a rate ceiling. Any target can stall: a share,
a busy disk, a virus scanner. The reader must not be the thread that waits.

## Decision

`RecordingWriter` owns the `PdqWriter` on a thread of its own. `record_frame`
hands over a frame through a bounded queue and returns immediately.

- The queue holds `WRITER_QUEUE_FRAMES = 1024` frames — about 8 s of stream at
  500 kSa/s, 4 MB of memory — so storage may stall for seconds without the
  reader ever waiting.
- `try_send` is used, never `send`. A full queue is reported as
  `write_error` ("recording queue overflow after N samples"), which makes the
  receipt invalid and fails the point. It is never a reason to block: waiting
  would produce exactly the segmented file this ADR exists to prevent.
- The writer thread keeps draining after a write failure, so a broken target
  cannot make the queue fill up and stall the reader indirectly.
- `finish` closes the queue, joins the thread, and only then flushes and
  finalizes the file. Finalizing runs on the caller's thread, which is the
  plugin thread at the end of a point — not the reader.
- The evidence writer's buffer grows from the `BufWriter` default of 8 KiB to
  1 MiB, so a recording reaches storage in about one write per second instead
  of some sixty.

## Consequences

- A storage stall no longer costs samples. It costs queue depth, and only a
  stall longer than the queue costs the recording — with a named error instead
  of a silent hole.
- `samples_written` and the marker counts now count **enqueued** frames. They
  stay truthful for a recording that finalizes cleanly; a recording that does
  not is invalid anyway, through `write_error`.
- Memory per active recording rises by up to 4 MB. Only one recording is open
  at a time.
- The written bytes are unchanged: the same frames in the same order, and the
  file-level CRC32/SHA-256 are still computed by the writer.
- This removes the host from the critical path, not the firmware's two-block
  ceiling. A stall in the operating system's serial stack still costs blocks,
  and the drop counter in the sidecar remains the check on that.
