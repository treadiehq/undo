#!/usr/bin/env python3
"""Generate an original, deterministic electronic score for the Undo video."""

from __future__ import annotations

import math
import struct
import wave
from pathlib import Path

SAMPLE_RATE = 44_100
DURATION = 42.0
TAU = math.tau
BEAT = 60 / 118
OUTPUT = Path(__file__).resolve().parents[1] / "public" / "undo-score.wav"

CHORDS = (
    (73.42, 110.00, 146.83),  # D major
    (61.74, 92.50, 123.47),   # B minor
    (49.00, 73.42, 98.00),    # G major
    (55.00, 82.41, 110.00),   # A major
)

ARPEGGIO = (
    293.66,
    369.99,
    440.00,
    587.33,
    246.94,
    293.66,
    369.99,
    493.88,
)

SCENE_CUES = (0.0, 3.5, 8.5, 13.5, 19.5, 26.5, 32.5, 37.0)


def clamp(value: float, low: float = -1.0, high: float = 1.0) -> float:
    return min(high, max(low, value))


def smoothstep(edge0: float, edge1: float, value: float) -> float:
    progress = clamp((value - edge0) / (edge1 - edge0), 0.0, 1.0)
    return progress * progress * (3 - 2 * progress)


def pseudo_noise(index: int) -> float:
    # Fast deterministic noise without storing a random buffer.
    value = math.sin(index * 12.9898 + 78.233) * 43_758.5453
    return (value - math.floor(value)) * 2 - 1


def cue_signal(time: float) -> tuple[float, float]:
    left = 0.0
    right = 0.0
    for cue_index, cue in enumerate(SCENE_CUES):
        age = time - cue
        if 0 <= age < 1.25:
            impact = math.exp(-age * 7.5)
            body = math.sin(TAU * (72 + cue_index * 2) * age) * impact
            shimmer = (
                math.sin(TAU * 960 * age + age * age * 110)
                * math.exp(-age * 4.2)
                * 0.11
            )
            left += body * 0.22 + shimmer
            right += body * 0.22 - shimmer * 0.72

        lead = cue - time
        if 0 < lead < 0.55:
            sweep = smoothstep(0.55, 0.0, lead)
            airy = math.sin(TAU * (360 + 1_250 * sweep) * time) * sweep * 0.024
            left += airy
            right -= airy

    return left, right


def sample_at(index: int) -> tuple[float, float]:
    time = index / SAMPLE_RATE
    fade_in = smoothstep(0.0, 1.1, time)
    fade_out = 1 - smoothstep(DURATION - 2.5, DURATION, time)
    master_envelope = fade_in * fade_out

    chord_index = min(len(CHORDS) - 1, int(time / 10.5))
    chord = CHORDS[chord_index]
    pad_left = 0.0
    pad_right = 0.0
    for tone_index, frequency in enumerate(chord):
        drift = 1 + math.sin(TAU * (0.045 + tone_index * 0.007) * time) * 0.003
        phase = TAU * frequency * drift * time
        pad = (
            math.sin(phase)
            + math.sin(phase * 2.002) * 0.24
            + math.sin(phase * 0.501) * 0.18
        )
        pan = (-0.34, 0.12, 0.38)[tone_index]
        pad_left += pad * (1 - pan) * 0.032
        pad_right += pad * (1 + pan) * 0.032

    rhythm_gain = smoothstep(3.6, 6.0, time) * (
        1 - smoothstep(35.5, 39.5, time)
    )
    beat_position = time / BEAT
    beat_phase = (beat_position - math.floor(beat_position)) * BEAT
    kick_envelope = math.exp(-beat_phase * 10.5)
    kick_frequency = 58 + 52 * math.exp(-beat_phase * 28)
    kick = (
        math.sin(TAU * kick_frequency * beat_phase)
        * kick_envelope
        * 0.24
        * rhythm_gain
    )

    half_beat = BEAT / 2
    pulse_position = time / half_beat
    pulse_index = int(pulse_position)
    pulse_age = (pulse_position - pulse_index) * half_beat
    pulse_frequency = ARPEGGIO[pulse_index % len(ARPEGGIO)]
    pulse_envelope = math.exp(-pulse_age * 8.4)
    pulse = (
        (
            math.sin(TAU * pulse_frequency * pulse_age)
            + math.sin(TAU * pulse_frequency * 2 * pulse_age) * 0.22
        )
        * pulse_envelope
        * 0.082
        * rhythm_gain
    )
    pulse_pan = math.sin(pulse_index * 1.7) * 0.42

    eighth_phase = ((time + half_beat) % BEAT)
    hat_envelope = math.exp(-eighth_phase * 42)
    hat = (
        pseudo_noise(index)
        * hat_envelope
        * 0.038
        * rhythm_gain
        * (0.7 + 0.3 * math.sin(TAU * 7_400 * time))
    )

    cue_left, cue_right = cue_signal(time)
    low_motion = math.sin(TAU * 41.2 * time) * 0.032

    left = (
        pad_left
        + kick
        + pulse * (1 - pulse_pan)
        + hat * 0.72
        + cue_left
        + low_motion
    )
    right = (
        pad_right
        + kick
        + pulse * (1 + pulse_pan)
        + hat
        + cue_right
        + low_motion
    )

    # Soft saturation keeps the generated mix controlled.
    return (
        math.tanh(left * 1.28) * master_envelope * 0.86,
        math.tanh(right * 1.28) * master_envelope * 0.86,
    )


def main() -> None:
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    frame_count = int(SAMPLE_RATE * DURATION)

    with wave.open(str(OUTPUT), "wb") as wav:
        wav.setnchannels(2)
        wav.setsampwidth(2)
        wav.setframerate(SAMPLE_RATE)

        buffer = bytearray()
        for index in range(frame_count):
            left, right = sample_at(index)
            buffer.extend(
                struct.pack(
                    "<hh",
                    int(clamp(left) * 32_767),
                    int(clamp(right) * 32_767),
                )
            )
            if len(buffer) >= 256 * 1024:
                wav.writeframesraw(buffer)
                buffer.clear()

        if buffer:
            wav.writeframesraw(buffer)

    print(f"Generated {OUTPUT} ({DURATION:.0f}s, stereo, {SAMPLE_RATE}Hz)")


if __name__ == "__main__":
    main()
