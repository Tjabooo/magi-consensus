# Changelog

## 0.6.0 (2026-11-16)

- Barge-in: the wake word keeps being listened for while MAGI is busy. Saying it again cuts her voice off
  at once and drops what she was doing (a request still transcribing or planning never runs its command
  or speaks); then she listens for the new request. "MAGI" alone or "MAGI, stop / never mind / tyst /
  glöm det / 算了" just silences her (and stops a writer still typing). `ears.barge_in` (default on).
- The talk key (`ptt start`) interrupts too, and `consensus hush` does it from a script or keybind.
- Voices run in their own process group (`tts.pid`), so the whole TTS pipeline stops at once.

## 0.5.1 (2026-11-16)

- Ears: Whisper's other spellings of the name ("Maggi", "Maggy", "Megi": one edit from magi/maggi/maggie,
  not real words like "magic") count as the wake word. "Maggi, …" requests were thrown away as false wakes.
- Transcripts are joined from Whisper's segments, which can split a word ("sk\nicka" → "skicka").
- `consensus write --to NAME` opens NAME's DM or channel in Discord (Ctrl+K switcher, checked by the
  window title; nothing is typed if it didn't open).
- A brief that is just the words ("hej på dig", "lol") is typed as said instead of being embellished.

## 0.5.0 (2026-11-16)

- `consensus write`: "write an essay about X" composes the text with the model and types it (streamed,
  line by line) instead of typing the description; dictated words ("type hello there") still go in
  verbatim. Writes in the spoken (or asked-for) language, fits the target (chat limit, one line in
  terminals/games), stops and copies to the clipboard if the focus moves, `--send` presses Enter after,
  `write --stop` / "stop writing" ends it, `write --print` tests it. `[write] model` / `busy_model`.
- Typing commands are repaired before rules and votes: `'I'm'` is re-quoted, Enter right after a
  `consensus write` becomes `--send`, Enter/`--send` without a send word in the request is dropped.
- Allow rules accept `'\''` escapes and type-then-Enter.

## 0.4.0 (2026-11-16)

- Mixed-language speech: a second language Whisper hears (from 15 %) goes to the planner too
  ("spoken in Chinese and English"), so "你好吗, answer in English" answers in English.
- Reply-language rules: "from now on answer my Chinese in English" / "以后用英语回答我" set a lasting
  rule (zh → en, "*" = any language) that the code applies to every later request; "stop answering my
  Chinese in English" clears it.
- Standing notes: "remember that …" keeps a fact for good; the planner sees them in every prompt.
  Notes and rules need a lasting word in the request (`planner.note_cues`), so a one-off "answer in
  English" isn't kept. `consensus notes`, `consensus forget all`.

## 0.3.0 (2026-11-16)

- General voice assistant: questions, small talk, statements and garbled speech get spoken answers;
  only words that ask for an action become a command. The planner first writes those `action_words`;
  a command with none is planned again as conversation.
- Fix: the planner's JSON keys were generated in alphabetical order (serde_json sorted the schema), so
  the model wrote `command` before deciding `kind`. Schema order is kept now (`preserve_order`).
- Replies in the language you spoke: Whisper's detected language goes to the planner, `reply_language`
  picks a voice from `[voice] tts_voices` (checked against the reply text).
- `[voice] language_remap`: a misdetected language is transcribed again as another (`da = "sv"`).
- Conversation memory: the last `planner.memory_turns` exchanges within `memory_minutes`;
  `consensus forget` clears it. `consensus hear` prints the language; `ask --lang CODE` for testing.
- Answers are spoken, never typed or sent into a window unless you asked to type or send.

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
