# Development

## Architecture

```
hold Right Alt ──► voxkit module (fcitx5) ──unix socket──► voxkit daemon
                    │                                       (Qwen3-ASR, local)
                    ├─ IM registered an insert handler → IM inserts the text
                    │    新酷音 Plus: at the cursor, composition kept
                    └─ otherwise → committed when the IM is idle
```

- **daemon/** — `voxkit`, the recognition daemon (Rust, transcribe.cpp).
  Microphone capture, Silero VAD, Qwen3-ASR / Whisper, OpenCC `s2twp`.
  Origin: the voice proof of concept in
  [siygle/fcitx5-mcbopomofo `plus-voice`](https://github.com/siygle/fcitx5-mcbopomofo/tree/plus-voice)
  (commit `e0e2f0f`, Cursor Agent). See `daemon/README.md` and `daemon/NOTICE`.
- **module/** — the fcitx5 addon. Watches the hotkey (before the IM), shows
  status in the aux area, talks to the daemon, delivers the transcript.
  Public API: `voxkit_public.h`.

## Speed (benchmarks)

Measured on an Intel Core Ultra 5 135U (CPU; Hex found its iGPU slower),
5 s of speech, release → text, through the socket like the input method:

| | before | now |
|---|---|---|
| per dictation | 3.5–4.1 s | **0.95–1.0 s** |
| first use / after fcitx5 restart | +0.5 s model load | load starts at key down, overlapping speech (was: loaded at service start; `VOXKIT_PREWARM=1` restores that) |

What changed (see `daemon/voxkit/examples/bench.rs`, `daemon/bench-client.py`):
- Vocabulary prompt capped at 40 characters. The prompt is decoded on every
  run: 80 terms (325 chars) cost +1.25 s. `VOXKIT_PROMPT_CHARS` overrides.
- 8 ASR threads instead of 4 (−35%). `VOXKIT_THREADS` overrides.
- Model shared by all connections; loaded at key down and unloaded after
  `VOXKIT_IDLE_UNLOAD` idle seconds (default 600). See docs/TRANSLATION.md.
- OpenCC through libopencc in-process (~1 ms) instead of spawning `opencc` (~80 ms).
- 120 ms tail after release instead of 200 ms.

Hex (same model, same library) logs 0.8–1.1 s for 3–5 s clips on this machine.

## Integrating an input method

Nothing is required: while the IM's composition is empty, the module commits
the transcript itself (and refuses, with a notice, while it is composing).
To insert into a non-empty composition or add vocabulary:

```cpp
#include <voxkit_public.h>
FCITX_ADDON_DEPENDENCY_LOADER(voxkit, instance_->addonManager());

handle_ = voxkit()->call<fcitx::IVoxkit::registerInputMethod>(
    "chewing",                                   // InputMethodEntry unique name
    fcitx::VoxkitRegistration{
        [this](fcitx::InputContext *ic, const std::string &text) {
            return insert(ic, text);             // true = handled
        },
        [this]() { return userPhrases(); },      // optional vocabulary
        nullptr});                               // optional busy check
```

CMake: `find_package(Fcitx5ModuleVoxkit)` → `Fcitx5::Module::Voxkit`, and add
`voxkit` to `[Addon/OptionalDependencies]`.

Integrated: [siygle/chewing-plus](https://github.com/siygle/chewing-plus) (fcitx5, IBus and macOS; it bundles this daemon).

## Build & test

```bash
cmake -B build -G Ninja && cmake --build build
test/run-test.sh build                 # module vs. a fake daemon (no mic)
cargo test --manifest-path daemon/Cargo.toml
cd packaging && makepkg -fd            # Arch package
```

## Daemon settings (systemd)

`systemctl --user edit voxkit.service`:

| Variable | Default | Meaning |
|---|---|---|
| `VOXKIT_MODELS_DIR` | `~/.local/share/hex/models` | Model store (read-only) |
| `VOXKIT_MODEL` | `qwen3-asr-0.6b` | Preferred ASR model. If that file is missing, the daemon uses an installed one (typically whisper-tiny) and logs the fallback. |
| `VOXKIT_THREADS` | cores − 2, max 8 | ASR threads |
| `VOXKIT_PROMPT_CHARS` | 40 | Vocabulary prompt budget; 0 disables |
| `VOXKIT_IDLE_UNLOAD` | `600` | Unload the ASR model after this many idle seconds (`never` keeps it) |
| `VOXKIT_MT_IDLE_UNLOAD` | same as above | Idle unload for the translation model |
| `VOXKIT_TEST_WAV` | — | Replay a WAV instead of the microphone (benchmarks) |
