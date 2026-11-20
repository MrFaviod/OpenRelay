#!/usr/bin/env python3
"""Analyse a WAV recording taken from the `openrelay-mic` PipeWire source.

Pass criterion (the one the harness enforces):
  * RMS clearly above silence (>= 0.01 of full scale), and
  * the dominant FFT peak within +-5 Hz of the 440 Hz tone the fake phone sends.

Usage: analyze.py out.wav [--tone 440] [--window 0.5]
"""

import argparse
import json
import math
import sys
import wave

import numpy as np


def dominant_hz(samples: np.ndarray, rate: int) -> float:
    window = np.hanning(len(samples))
    spectrum = np.abs(np.fft.rfft(samples * window))
    if not spectrum.any():
        return 0.0
    index = int(np.argmax(spectrum))
    # Parabolic interpolation around the peak: much finer than one bin.
    if 0 < index < len(spectrum) - 1:
        left, mid, right = spectrum[index - 1], spectrum[index], spectrum[index + 1]
        denom = left - 2 * mid + right
        offset = 0.5 * (left - right) / denom if denom != 0 else 0.0
    else:
        offset = 0.0
    return (index + offset) * rate / len(samples)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("wav")
    parser.add_argument("--tone", type=float, default=440.0)
    parser.add_argument("--window", type=float, default=0.5)
    args = parser.parse_args()

    with wave.open(args.wav, "rb") as wav:
        rate = wav.getframerate()
        channels = wav.getnchannels()
        width = wav.getsampwidth()
        frames = wav.getnframes()
        raw = wav.readframes(frames)

    if width != 2:
        print(f"FAIL: sample width {width * 8} bits, expected 16")
        return 1
    data = np.frombuffer(raw, dtype="<i2").astype(np.float64)
    if channels > 1:
        data = data.reshape(-1, channels).mean(axis=1)
    if data.size == 0:
        print("FAIL: recording is empty")
        return 1

    full_scale = float(np.iinfo(np.int16).max)
    normalized = data / full_scale
    rms = float(np.sqrt(np.mean(normalized**2)))
    rms_db = 20.0 * math.log10(max(rms, 1e-9))
    peak_amplitude = float(np.max(np.abs(normalized)))
    peak_hz = dominant_hz(normalized, rate)

    window_len = max(1, int(args.window * rate))
    rms_windows = []
    for start in range(0, len(normalized) - window_len + 1, window_len):
        chunk = normalized[start : start + window_len]
        rms_windows.append(round(float(np.sqrt(np.mean(chunk**2))), 4))

    non_silent = sum(1 for value in rms_windows if value >= 0.01)
    result = {
        "file": args.wav,
        "rate_hz": rate,
        "channels": channels,
        "frames": int(data.size),
        "duration_s": round(data.size / rate, 3),
        "rms": round(rms, 5),
        "rms_db": round(rms_db, 2),
        "peak_amplitude": round(peak_amplitude, 5),
        "dominant_hz": round(peak_hz, 2),
        "tone_hz": args.tone,
        "tone_error_hz": round(peak_hz - args.tone, 2),
        "rms_windows": rms_windows,
        "non_silent_windows": non_silent,
    }
    print(json.dumps(result, indent=2))

    ok = rms >= 0.01 and abs(peak_hz - args.tone) <= 5.0
    if ok:
        print(
            f"PASS: RMS={rms_db:.2f} dBFS (linear {rms:.4f}, >=0.01) "
            f"peak={peak_hz:.2f} Hz "
            f"({peak_hz - args.tone:+.2f} Hz vs {args.tone:.0f} Hz)"
        )
        return 0
    if rms < 0.01:
        print(
            f"FAIL: RMS={rms_db:.2f} dBFS (linear {rms:.5f}) is silence or near-silence"
        )
    else:
        print(
            f"FAIL: dominant peak {peak_hz:.2f} Hz is outside "
            f"+-5 Hz of {args.tone:.0f} Hz"
        )
    return 1


if __name__ == "__main__":
    sys.exit(main())
