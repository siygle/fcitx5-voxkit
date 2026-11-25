# Voice translation (experimental)

Hold the translate key, speak, let go: the daemon recognizes the speech as
usual, then translates the transcript with a local model and the input
method inserts the translation. Off by default.

## Design

```
key down ─► start {mode:"translate", target:"en"} ─► daemon: record
                                                     preload ASR + MT model
key up   ─► stop ─────────────────────────────────► ASR ─► translate ─► final
```

- **No extra process.** Translation is a stage inside `voxkit serve`, after
  ASR. There is no llama-server.
- **Optional plugin.** `libvoxkit-mt.so` (`.dylib` on macOS) is llama.cpp plus
  its *own* ggml, linked statically with hidden visibility; only five C
  functions are exported (`daemon/mt-plugin/voxkit_mt.h`). The daemon
  `dlopen`s it (`RTLD_LOCAL`) on the first translate request and `dlclose`s
  it on idle unload. transcribe-cpp's ggml 0.26 inside the daemon and the
  plugin's ggml never meet, so each can be upgraded alone, and the daemon
  binary does not grow. llama.cpp is pinned in `daemon/mt-plugin/LLAMA_PIN`.
- **Model.** `hy-mt1.5-1.8b`: `tencent/HY-MT1.5-1.8B-GGUF`, Q4_K_M,
  1 133 080 512 bytes, SHA-256 `4383ac0c…c477c7`, pinned revision `265b2e6`.
  Tencent HY Community License (excludes the EU, UK and South Korea).
  Not bundled. The IME's first translate request commits the original
  transcript immediately and starts a background download (status
  「翻譯模型下載中」). The CLI does the same (prints the original, exit 3)
  unless you pass `--wait` or run `voxkit model install hy-mt1.5-1.8b`
  first. `VOXKIT_MT_MODEL` picks another `mt` catalog entry.
- **Prompt.** The model card's templates: Chinese template when Chinese is on
  either side (`将以下文本翻译为英语，…`), English template otherwise. Greedy
  decoding, top-k 40, repetition penalty 1.05, output ≤ 4×input+64 tokens.
- **Traditional Chinese.** Targets `zh-Hant` / `zh-TW` get OpenCC `s2twp`
  after translation.
- **Failure = original text.** No plugin, model still downloading, load or
  decode error, empty output, unknown target: the final carries the original
  transcript (converted as in dictation), `translated:false` and
  `translate_error` (`plugin-missing`, `model-downloading`, `load-failed`,
  `translate-failed`, `empty`, `bad-target`).
- **User dictionary.** Still sent as the ASR vocabulary prompt (it helps
  recognize names). It is never applied to the translation.
- **Lazy load / idle unload, both models.** Nothing is loaded at daemon
  start. `start` begins loading the ASR model (and, for translate, the MT
  model) in the background while you speak. A model idle for
  `VOXKIT_IDLE_UNLOAD` seconds (default 600; `never` keeps it) is unloaded;
  `VOXKIT_MT_IDLE_UNLOAD` overrides it for translation. A request holds its
  models from `start` until its `final` (or cancel), and the reaper only
  unloads with the model's lock free and nothing in flight.
  `VOXKIT_PREWARM=1` restores loading the ASR model at startup.

## Protocol (v1, additive)

- `start` gains `mode` (`"dictate"` default | `"translate"`) and `target`
  (default `"en"`).
- `final` for translate adds `mode:"translate"`, `target`, `translated`,
  `source` (the transcript) and, on fallback, `translate_error`. Dictation
  finals are unchanged.
- `model.status` (and `model.list`) gain `capabilities: ["translate"]`;
  `model.status` also has `translate: {plugin, plugin_path, model, installed,
  downloading, targets}`.
- An older daemon ignores `mode`/`target` and would dictate. **Clients must
  see `"translate"` in `capabilities` before sending `mode:"translate"`.** The
  fcitx5 module asks once per connection (synchronous `model.status`, req 0,
  ≤ 400 ms) and refuses the key with a notice otherwise.

## fcitx5 module settings (`conf/voxkit.conf`)

| key | default | |
|---|---|---|
| `TranslateEnabled` | `False` | feature switch |
| `TranslateHotkey` | `Control_R` | hold to translate (modifiers only; Right Shift is 新酷音 Plus's English double tap) |
| `TranslateTarget` | `en` | `en ja ko zh-Hant zh-Hans fr de es pt it ru vi th id ms tr ar nl pl` |

## Try it without a microphone

```sh
voxkit model install hy-mt1.5-1.8b            # skip this to see the fallback
VOXKIT_MT_PLUGIN=/usr/lib/voxkit/libvoxkit-mt.so \
  voxkit translate --target en 今天天氣很好，我們去公園散步
# missing model → original text, exit 3 (does not start a download)
# pass --wait to download first, then translate (exit 0 on success)
voxkit transcribe clip.wav --model qwen3-asr-0.6b --translate ja
```

Exit status 0 = translated, 3 = fell back to the original.

The IME path never blocks on a missing translation model: `start mode=translate`
commits the ASR transcript at once (`translated:false`,
`translate_error:model-downloading`) and the frontend shows 「翻譯模型下載中」.
A later request, once the file is in the store, is translated.

## Speech model default

The systemd unit sets `VOXKIT_MODEL=qwen3-asr-0.6b` as the *preferred* ASR
model. If that file is not in the store, the daemon uses any installed ASR
model (typically `whisper-tiny`) and logs the fallback. Install the preferred
one with `voxkit model install qwen3-asr-0.6b`. If *no* ASR model is
installed, the first dictation starts a download of the small default
(`whisper-tiny`), not the 850 MB preferred model.

## Automated daemon checks

`cargo test --manifest-path daemon/Cargo.toml --release --test live_daemon`
talks to a real `voxkit serve` (needs models and the plugin; skipped when
they are absent). Defaults look under `/workspace/vkmt-work/`. Override with
`VOXKIT_ITEST_STORE`, `VOXKIT_MT_PLUGIN`, `VOXKIT_ITEST_WAV`.

## Build the plugin

```sh
git clone https://github.com/ggml-org/llama.cpp && git -C llama.cpp checkout "$(cut -d' ' -f2 daemon/mt-plugin/LLAMA_PIN)"
cmake -S daemon/mt-plugin -B build-mt -G Ninja -DCMAKE_BUILD_TYPE=Release \
  -DLLAMA_CPP_SOURCE_DIR=$PWD/llama.cpp -DGGML_NATIVE=OFF -DGGML_AVX2=ON -DGGML_FMA=ON -DGGML_F16C=ON
cmake --build build-mt && sudo cmake --install build-mt --prefix /usr   # /usr/lib/voxkit/libvoxkit-mt.so
nm -D --defined-only build-mt/libvoxkit-mt.so   # only vk_mt_*
```

## Log lines (journalctl --user -u voxkit)

Real output from `voxkit serve` on a 4-core x86-64 box (whisper-tiny ASR,
HY-MT1.5-1.8B Q4_K_M, plugin built at llama.cpp b12722), two translate
requests and one dictation, then the idle reaper (here with
`VOXKIT_IDLE_UNLOAD=8`):

```
voxkit: models load on first use; idle unload: speech after 600 s, translation after 600 s
voxkit: translation plugin available: /usr/lib/voxkit/libvoxkit-mt.so
voxkit: listening on /run/user/1000/voxkit.sock
voxkit: req 2 start mode=translate target=en
voxkit: translation plugin /usr/lib/voxkit/libvoxkit-mt.so (llama.cpp b12722 ggml 1.0.0)
voxkit: loading whisper-tiny
voxkit: preloaded whisper-tiny in 237 ms
voxkit: loaded translation model hy-mt1.5-1.8b in 1169 ms
voxkit: translate zh -> en (16 chars) in 1067 ms: ok
voxkit: req 2 translate target=en translated=true error=-
voxkit: req 3 start mode=translate target=ja
voxkit: translate zh -> ja (16 chars) in 1430 ms: ok
voxkit: req 3 translate target=ja translated=true error=-
voxkit: req 4 start mode=dictate target=-
voxkit: unloaded speech model after 8 s idle
voxkit: unloaded translation model after 8 s idle
```

The finals for req 2 and 3 (that box had no `opencc` on PATH, so `source`
stayed Simplified; with opencc installed it is the s2twp text the user would
have got from dictation):

```
{"mode":"translate","req":2,"source":"今天天气很好我们下午去公圆散步吧","target":"en","text":"The weather is very nice today. Let's go for a walk in the park this afternoon.","translated":true,"type":"final","v":1}
{"mode":"translate","req":3,"source":"…","target":"ja","text":"今日の天気はとても良いです。午後には公園を散歩しに行きましょう。","translated":true,"type":"final","v":1}
```

Peak RSS with both models loaded is about 2.0 GB (the GGUF is mmapped, so
most of it is page cache); after the idle unload the daemon is back to
about 60 MB.
