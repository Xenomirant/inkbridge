# ADR-0007: Handwriting recognition in tiers behind a `Recognizer` trait

- Status: Accepted (implementation in M5)
- Date: 2026-11-16

## Context
Research (2026-11) found no mature open-source, open-weights **stroke-based** recogniser covering English, Chinese and Russian. Most stroke datasets (IAM-OnDB, CASIA-OLHWDB, MathWriting) are non-commercial or research-only. Users need offline, free recognition, including in China, where Google services are unreliable.

## Decision
- `ib-ink` stores strokes as vectors (points, pressure, time) independent of video; recognition never depends on pixels.
- `Recognizer` trait returns **ranked candidates linked to strokes**, enabling correction (pick an alternative, re-recognise).
- **Correction UI ships first:** scratch-out to delete, join/split, strike-to-select, candidate list; gestures can be turned off.
- Tier 1 — OS recognisers where they exist: Windows Ink (strokes sent to the host), iPadOS `PKStrokeRecognizer`; ML Kit Digital Ink optional on Android with Google services.
- Tier 2 — bundled open models on **tract** (pure Rust; runs on older CPUs and Intel Macs): Zinnia + Tegaki for single CJK characters; our own small LSTM-CTC line model (en/ru/zh); optional CoMER-style math model.
- Tier 3 — re-ranking with an on-device n-gram model and a user dictionary.
- Model weights are licensed separately from the code, with their dataset terms documented.

## Consequences
- + Works offline and free on every platform, with OS-quality results where the OS provides them.
- + Recognition quality can improve without changing the UI or protocol.
- − Training our own model is real work, and dataset licences limit how weights can be redistributed.
- − Results differ by platform until the bundled model is good enough.
