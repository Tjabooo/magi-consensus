# MAGI Consensus: design note

A local voice assistant that never acts alone. You ask for something, a planner model turns it into
one concrete action, and three independent "units" vote on it. The action runs only with 2 of 3
approvals. Everything runs on your own machine against an OpenAI-compatible endpoint
(llama.cpp's `llama-server`, npukit's gateway, Ollama, ...).

## Goals

- **Two-of-three rule.** No single model output can cause a side effect.
- **Hard rules before votes.** Some things are never up for a vote: privilege escalation is always
  refused, destructive commands only run when they match an allowlist you wrote yourself.
- **Free when idle.** No daemon. Each request is one short-lived process; the model server can unload
  its models after a while (npukit does this on its own).
- **Generic.** Endpoint, models, hotkey commands, personas, tools, rules, TTS: all in config.

## Pipeline

```
 hotkey down ──► record (pw-record) ──► hotkey up ──► Whisper (/v1/audio/transcriptions)
                                                            │
 `consensus ask "text"` ────────────────────────────────────┤
                                                            ▼
                                     planner (JSON-schema constrained) → one action
                                                            │
                         answer (no side effect) ◄──────────┤
                         is spoken directly                 ▼
                                                     hard rules ── DENY ─► card + voice, logged
                                                            │ pass
                                                            ▼
                               MELCHIOR · BALTHASAR · CASPER vote (in parallel)
                                                            │
                                         ≥2 APPROVE ── run (sh -c, timeout, capped output)
                                                            │
                                    vote card (dunst) + spoken reply + votes.jsonl + last.json
```

### Actions

The planner returns exactly one of:

| kind      | fields                                  | voted? |
|-----------|-----------------------------------------|--------|
| `shell`   | `command`, `reply`, `speak_output`      | yes    |
| `answer`  | `reply` (just talk, nothing runs)       | no     |
| `none`    | `reply` (didn't understand / can't)     | no     |

`shell` covers every integration on purpose (`hyprctl`, `wpctl`, `playerctl`, `curl` to a local API,
your own scripts): the planner learns what exists from the `[[tools]]` list in the config, so adding a
capability is a config edit, not code.

### Hard rules (no vote)

Commands are normalised (quotes and backslashes stripped), then split into words on whitespace and
shell metacharacters, so `bash -c "s''udo x"` or `$(sudo ...)` still yield the word `sudo`.

1. **Never**: any word in `rules.never` (default: `sudo doas su pkexec run0`) → denied, always.
2. **Destructive**: any word in `rules.destructive` (`rm dd mkfs shred kill systemctl reboot ...`),
   a pattern in `rules.destructive_patterns` (`git push --force`, `> file` overwrites ...) →
   denied unless the whole command matches a regex in `rules.allow`.
3. **Opaque**: interpreters that hide what runs (`sh bash eval python ...`) count as destructive.

Rules over-approximate on purpose: a false DENY costs a retry, a false PASS could cost data.

### The three units

Same Evangelion split as the MAGI panel: three facets of one mind, each with its own question.

| unit      | persona      | asks                                                         |
|-----------|--------------|--------------------------------------------------------------|
| MELCHIOR  | practical    | Will this command actually do what was asked, correctly?     |
| BALTHASAR | cautious     | Could this hurt the machine, data, privacy or anything running? |
| CASPER    | intent       | Is this what the person *meant*, not just what they said?    |

Each gets the request, the proposed command, the planner's reasoning and the context snapshot, and
must answer `{"verdict": "APPROVE"|"DENY", "reason": "<one line>"}` (JSON-schema constrained, thinking
off, ~60 tokens). A voter that errors or times out counts as DENY: failure never approves.
Each unit's model and temperature are configurable, so they can be the same model (default; one
model in VRAM) or three different ones.

### Context

`[context] commands` (e.g. `hyprctl activewindow -j`, `date`) run before planning; their trimmed
output goes to the planner and voters ("the focused window is CS2" changes what "mute it" means).

### Output

- **Vote card**: a notification (`notify-send`, replaced in place by a stack tag) with the request,
  command, and one line per unit: verdict + reason; result line on top (`APPROVED 3/3`, `DENIED 1/3`,
  `REFUSED by rule: sudo`).
- **Voice**: optional TTS command fed the reply on stdin (Piper + `pw-play` in the example config).
- **State**: `$XDG_STATE_HOME/magi-consensus/votes.jsonl` (one line per request: input, action, rule
  result, votes, exit code, timings) and `last.json` (for a status bar / eww card later).

### Push to talk

`consensus ptt start` spawns the recorder and returns; `consensus ptt stop` stops it (SIGINT so the
WAV header is written), transcribes and runs the pipeline. Bind start on key press and stop on key
release. `start` while already recording acts as `stop`.

Release binds are not reliable: Hyprland skips a `SUPER + ALT + X` release bind when a modifier comes
up before X, which left the recording running and the card on "listening…". So `start` also spawns a
watcher (`ptt watch PID`) that reads `voice.hold_key` from every keyboard through evdev (EVIOCGKEY,
read-only, needs the `input` group). If the key was held at start and has been up for 250 ms with no
stop, the watcher stops the recording itself; it also finishes a recording that hit the
`max_seconds` cap instead of dropping it. The bind and the watcher race for the recording by renaming
`ptt.json`, so only one of them transcribes it. Watcher notes go to `~/.local/state/magi-consensus/ptt.log`.
Quiet recordings (RMS below `min_rms`) are dropped: Whisper invents text from silence.

## Not in v0.1

- Multi-step plans / tool loops (one action per request, on purpose).
- Wake word. Conversation memory between requests.
- Eww vote card in the MAGI panel (`last.json` is there for it).

## Fast path (0.2.0)

The council is slow on purpose (~5 s) and that's wrong for "clip that" in the middle of a fight. Phrases
you wrote yourself in `[[commands]]` skip it: the request is normalised (lower case, number words →
digits, wake words and fillers stripped), tried whole against every phrase, then split on "and" /
"then" / punctuation. It takes the fast path only when every part matches; one unknown part sends the
whole request to the planner, so a half-understood chain never half-runs. No vote is needed because
nothing here was invented by a model: the command text is yours, slots are digits or resolved app
windows, and `{text}` is shell-quoted.

## Ears (0.2.0)

`pw-record` → 100 ms chunks → sherpa-onnx keyword spotter (zipformer transducer, 3.3 M parameters,
int8, one CPU thread) with your keywords tokenised from the model's `bpe.model`. On a hit, the last
1.5 s (the wake word itself) plus everything until 0.8 s of quiet (noise floor × `speech_factor`) is
sent to Whisper. Whisper has to hear a wake word in its first words too; a keyword-spotter false alarm
("magic") then costs one transcription and nothing runs. While a request is handled, the spotter is
paused, so the spoken answer can't wake it.

### Conversation, languages, memory (0.3.0)

The planner is a general assistant first: its JSON starts with `action_words` (the words that ask for
an action on the computer), then `kind`. Key order matters: llama.cpp generates in the schema's order,
and serde_json keeps it only with `preserve_order` (before 0.3.0 the keys were sorted, so `command` came
before `kind`). A `shell` plan with empty `action_words` is planned once more with a note that it is
conversation. Whisper runs with `verbose_json`; the most likely language code goes to the planner
("spoken in Swedish; reply in Swedish" when a voice exists), and `[voice] language_remap` transcribes a
misdetected language again as another. `reply_language` is checked against the text (CJK or not,
Swedish letters/words) before it picks a `tts_voices` entry. Recent exchanges live in
`conversation.jsonl` (state dir, last 50 lines) and go into the planner's user message.

### Mixed languages, rules and notes (0.4.0)

`Heard.langs` holds the top language plus a second one at 15 % or more (Whisper's
`language_probabilities`); the planner sees "spoken in Chinese and English". Lasting preferences are
stored by the code, not left to the model: `rule_heard`/`rule_reply` become `reply-languages.json`
(heard → reply, `*` = any), applied in the request line ("reply in English (the user's standing
rule)"); an empty `rule_heard` means the language the request was spoken in. `remember`/`forget`
edit `notes.json` (max 30), which goes into every planner prompt. Both only change when the request has
a word from `planner.note_cues`, so a one-off "answer in English" is not kept; clearing a rule needs
none. Notes under three words are replaced by the request's own words.
