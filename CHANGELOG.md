# Changelog

## 0.2.0 (2026-11-15)

- Fast path: your own `[[commands]]` phrases run at once with no model or vote; requests chain with
  "and"/"then", and only go fast when every part matches. Built-in window/workspace/media commands.
- `[[apps]]`: names and aliases → windows (class, title, or a process below the window) and launch
  commands; open windows match by class; "it" = the previous step's app, waiting for it to launch.
- `consensus ears`: always-on wake word with a sherpa-onnx keyword spotter on the CPU, end-of-speech
  detection, Whisper wake-word check against false wakes; `on/off/toggle/status/test`.
- `consensus commands`, `ask --vote`. Your `[[commands]]` extend the built-ins.

## 0.1.0 (2026-11-14)

- First release: planner → hard rules → three-unit vote (2 of 3) → run, vote card, votes.jsonl.
- Push to talk (`pw-record` + Whisper endpoint), TTS pipeline (Piper), `check`, `log`, `hear`.
