# Latency targets (not measured)

**Status: proposed targets, not measurements.** There is no physical baseline
yet: this document sets targets and ceilings so they can be measured later, and
declares the method for each number. No number here has been achieved,
benchmarked, or observed on a device. The project's priority is LAN latency.

## Per-stage budget (proposed)

End-to-end path for the v1 frame (PCM16LE mono 48 kHz, 5 ms per
frame). `budget_us` is the design target and `ceiling_us` the acceptable
ceiling; both are proposals.

| Stage | Target | Ceiling | Method / note |
|---|---:|---:|---|
| Capture | 5 ms | 12 ms | source device buffer; not measured |
| Frame | 5 ms | 5 ms | fixed duration of the v1 profile; not measured processing |
| LAN (one way) | 3 ms | 15 ms | the project's priority; no measured network figures |
| Jitter | 10 ms | 40 ms | default target; 40 ms is the contract maximum |
| Resample + output | 2 ms | 8 ms | adaptive resample and delivery to the sink; not measured |
| **Total** | **25 ms** | **80 ms** | sum of stages, not observed latency |

The Jitter row uses the default target of `openrelay-audio-core` (10 ms) and its
contract maximum (40 ms); it is not a measured wait. The LAN row is the
priority: if a future measurement misses it, the order of attack is network →
jitter → capture.

The block below is the sum of the contract's stages; it does not measure latency.

<!-- verify:latency-budget -->
```json
{
  "status": "proposed_not_measured",
  "end_to_end_budget_us": 25000,
  "end_to_end_ceiling_us": 80000,
  "stages": [
    {"stage": "capture", "budget_us": 5000, "ceiling_us": 12000},
    {"stage": "frame", "budget_us": 5000, "ceiling_us": 5000},
    {"stage": "lan", "budget_us": 3000, "ceiling_us": 15000},
    {"stage": "jitter", "budget_us": 10000, "ceiling_us": 40000},
    {"stage": "resample_output", "budget_us": 2000, "ceiling_us": 8000}
  ]
}
```

## Measurement

The budget above remains a **proposal, not a measurement**. Physical
measurement requires a reference device, a capture with `pw-record` and
evidence stored with a hash; until then there are no results.

- **End-to-end latency:** p50/p95/p99 by click detection and
  cross-correlation between reference and `pw-record` capture. The analysis
  resolution is 1 sample (0.0208 ms at 48 kHz); the physical uncertainty
  is declared separately.
- **RTT:** measured and reported in its own `rtt_ms` field, separate from
  `latency_ms`. It is not added to, subtracted from or averaged with latency;
  confusing them invalidates the evidence.
- **1 h drift:** `cargo test -p openrelay-audio-core --test drift_1h -- --ignored`
  is the simulated reference; hardware evidence requires a real 60 min stream
  with `ppm` and `resyncs`.
- **Evidence:** every capture is stored with its hash and the environment (model, OS,
  audio buffer and network point). Without a physical capture there is no figure.

Current state: **no physical measurement**. There is no physical baseline or
measured reference device; these figures are not presented as achieved. The README
publishes no latency figures: only this document mentions them, and as targets.
When valid evidence exists the `verify:latency-budget` block will be updated;
until then it keeps `proposed_not_measured` so as not to declare measurements that do
not exist.

## Idle and streaming counters

`openrelay_audio_core::budget` separates two accumulators that are never mixed:

- **Idle**: process alive with no audio in progress. Counts only what the consumer
  observes while the mode is idle.
- **Streaming**: audio in progress. Accumulates, by delta, the real metrics of
  `JitterBuffer` (frames emitted from the network, samples, gaps, PLC, drops) and of
  `DriftController` (resyncs). It does not reimplement the pipeline or create a second
  source of truth for counters.

The pipeline baseline is **single and shared**: each `observe`
accumulates the delta in the current mode and advances the other mode's baseline
without adding anything to it (`rebase`). So each delta is counted exactly once and
alternating `streaming → idle → streaming` does not reattribute emissions already counted: if
the buffer emitted frames 1, 2 and 3 and was observed in streaming, idle and streaming,
the accumulators report 2 in streaming and 1 in idle (sum 3), not 5.

Declared edge cases:

- **Repeated observation**: the same snapshot with no changes gives a zero delta in
  every field; it does not duplicate the pipeline.
- **Mode change between observations**: the unobserved interval is attributed
  to the mode in force at the next observation, because there is no intermediate sample
  to split it. To attribute the boundary precisely, observe before changing
  mode.
- **Saturation**: `frames`, `samples`, `gaps`, `plc_frames`, `dropped`,
  `resyncs`, `underruns` and `overruns` saturate at `u64::MAX`; a hostile stream
  neither overflows nor panics.
- **Pipeline restart**: if the counters read go backwards (new buffer or
  controller), the delta is counted from zero instead of being clamped to
  zero; the restart neither loses the new data nor subtracts what was accumulated. A restart that
  returns to a value equal to or greater than the baseline cannot be told apart by
  counters alone and is declared a limit of the method.

The `budget_idle_stream` tests cover mode separation, streaming→idle→streaming
alternation, repeated observation, restart and saturation.

## Resource ceilings (proposed)

| Resource | Ceiling | Unit | Measurement method |
|---|---:|---|---|
| RSS idle | 24 MiB | bytes | `VmRSS` from `/proc/<pid>/status` at rest |
| RSS streaming | 48 MiB | bytes | `VmRSS` from `/proc/<pid>/status` with audio |
| CPU idle | 10 ms/s | ms/s | `utime+stime` from `/proc/<pid>/stat` per interval |
| CPU streaming | 150 ms/s | ms/s | `utime+stime` from `/proc/<pid>/stat` per interval |
| Artifact | 8 MiB | bytes | deployed release binary, not `target/` |
| Own threads | 4 | threads | by name/role; ART/Compose/RenderThread separately |
| Control heap | 64 KiB | bytes | control thread; excludes preallocated audio |

The ceilings are proposal bounds. Any future measurement is compared against
`budget::check(resource, observed)`; an excess returns `Verdict::Over`, not an
approval.

## Memory

The memory figures are compile-time `size_of` of the inline state, not the wire
size of the frame (508 B) or RSS. An `AudioFrame` in Rust memory measures 492 B
and each slot adds the `Option` tag (512 B per slot), so multiplying 508 B
per slot does not describe the real memory.

| Inline state | Bytes (compile host) | What it counts |
|---|---:|---|
| `JitterBuffer` | 8872 | 16 slots with a frame, last frame for PLC and accounting; it is `AUDIO_PREALLOCATED_BYTES` |
| `LinearResampler` | 1048 | 488-sample ring and phase between frames |
| `DriftController` | 144 | configuration, ratio, integral and counters |
| **Audio path** | **10064** | `AUDIO_PATH_STATE_BYTES` = sum of the three |

- **Preallocated audio**: `AUDIO_PREALLOCATED_BYTES = size_of::<JitterBuffer>()`
  (8872 B). It is reserved once and does not grow with the stream.
- **Resampler and controller state**: declared separately
  (`RESAMPLER_STATE_BYTES`, `DRIFT_STATE_BYTES`) and only enter the total
  `AUDIO_PATH_STATE_BYTES`; they are not counted inside the jitter buffer.
- **Transient allocation on the audio path**: zero
  (`TRANSIENT_AUDIO_ALLOC_BYTES = 0`).
- **Control thread heap**: separate budget, bounded to 64 KiB. It does not
  share an account with preallocated audio.
- **Not counted**: allocator heap and metadata, thread stacks, system/Android
  buffers and RSS. Those figures are measured separately with `VmRSS` and are not
  part of these constants.

The table values are those of the compile host; the
constants are derived with `size_of` and the test fails if they stop matching.

## Android threads

Only the app's own threads enter the budget: capture, network,
jitter consumer and control thread (4). **ART, Compose and RenderThread are
external system threads**: they are measured separately with Perfetto or
`dumpsys`, and this budget neither adds them nor presents them as its own.

## What this document is not

- It is not a benchmark or a measurement of latency, CPU, memory or network.
- The 1 h drift simulation (`drift_1h`, marked `#[ignore]`) verifies the
  loop with simulated clocks; it is not a hardware or LAN test.
- It does not choose a reference device or phone: physical measurement is
  pending and its reference (model, OS, audio buffer, network point) will be
  recorded when measuring. The compile host is only the environment where the
  code is built and the tests run.
- It provides no achieved figures and does not turn a global criterion into an approval. The
  figures remain targets until independent review and physical
  measurement.
