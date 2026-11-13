# fcitx5-voxkit

Push-to-talk local dictation for **any** fcitx5 input method.

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

Everything runs locally; audio never leaves the machine. Disabled in
password / sensitive fields.

## Use

```bash
systemctl --user enable --now voxkit.service   # daemon
# fcitx5-configtool → Addons → Voxkit to change the key or turn it off
```

Hold **Right Alt**, speak, release. Holds shorter than 250 ms are ignored
(so a stray tap does nothing). Pressing any other key while holding cancels.

The user service reuses the model files the **Hex** app downloaded
(`~/.local/share/hex/models`, read-only; checksums are verified). To use
voxkit's own store instead: `systemctl --user edit voxkit.service` and set
`VOXKIT_MODELS_DIR`, then `voxkit model install qwen3-asr-0.6b`.

## Speed

Measured on an Intel Core Ultra 5 135U (CPU; Hex found its iGPU slower),
5 s of speech, release → text, through the socket like the input method:

| | before | now |
|---|---|---|
| per dictation | 3.5–4.1 s | **0.95–1.0 s** |
| first use / after fcitx5 restart | +0.5 s model load | 0 (loaded and warmed at service start) |

What changed (see `daemon/voxkit/examples/bench.rs`, `daemon/bench-client.py`):
- Vocabulary prompt capped at 40 characters. The prompt is decoded on every
  run: 80 terms (325 chars) cost +1.25 s. `VOXKIT_PROMPT_CHARS` overrides.
- 8 ASR threads instead of 4 (−35%). `VOXKIT_THREADS` overrides.
- Model loaded and warmed once at startup, shared by all connections.
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

Integrated: **fcitx5-chewing** `plus` branch
([siygle/fcitx5-chewing](https://github.com/siygle/fcitx5-chewing/tree/plus)).

## Build & test

```bash
cmake -B build -G Ninja && cmake --build build
test/run-test.sh build                 # module vs. a fake daemon (no mic)
cargo test --manifest-path daemon/Cargo.toml
cd packaging && makepkg -fd            # Arch package
```

## License

MIT. Model weights are not included and keep their own licenses.
