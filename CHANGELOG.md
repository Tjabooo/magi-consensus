# Changelog

## Unreleased

- Wake-word requests that aren't your own phrases run the planner's command after the hard rules
  without a council vote (`ears.vote = false`, the default); push to talk and `ask` still vote.
- Ears read Sun AU from `pw-record` without `--raw` correctly (big-endian); defaults use `--raw`.

## 0.2.1 (2026-11-16)

- Push to talk no longer gets stuck on "listening…": a watcher reads the hold key from the keyboard
  (evdev) and stops the recording when Hyprland drops the release bind (modifier released before X).
  Recordings that hit `max_seconds` are now transcribed instead of silently dropped. New `[voice] hold_key`.
- The card says "transcribing…" while Whisper works (it said "listening…").

## 0.2.0 (2026-11-15)

- Fast path: your own `[[commands]]` phrases run at once with no model or vote; requests chain with
  "and"/"then", and only go fast when every part matches. Built-in window/workspace/media commands.
- `[[apps]]`: names and aliases → windows (class, title, or a process below the window) and launch
  commands; open windows match by class; "it" = the previous step's app, waiting for it to launch.
- `consensus ears`: always-on wake word with a sherpa-onnx keyword spotter on the CPU, end-of-speech
  detection, Whisper wake-word check against false wakes; `on/off/toggle/status/test`.
- `consensus commands`, `ask --vote`. Your `[[commands]]` extend the built-ins.
- `consensus ears debug on|off`: saves every burst of speech (last 40) with whether it woke, to tune the wake word.

## 0.1.0 (2026-11-14)

- First release: planner → hard rules → three-unit vote (2 of 3) → run, vote card, votes.jsonl.
- Push to talk (`pw-record` + Whisper endpoint), TTS pipeline (Piper), `check`, `log`, `hear`.
