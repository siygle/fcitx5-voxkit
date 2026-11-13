//! Stage timing for one WAV: load, Silero VAD, ASR run (n threads), OpenCC.
//! cargo run --release --example bench -- FILE.wav MODEL_PATH VAD_PATH [threads...]
use std::time::Instant;
use transcribe_cpp::{init_backends_with, Backend, BackendMask, Model, ModelOptions, RunOptions, SessionOptions, VadOptions};

fn ms(t: Instant) -> f64 { t.elapsed().as_secs_f64() * 1000.0 }

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let pcm = voxkit::audio::load_wav_16k(std::path::Path::new(&a[1]))?;
    println!("audio {:.0} ms", pcm.len() as f64 / 16.0);
    transcribe_cpp::disable_logging();
    init_backends_with(None::<&std::path::Path>, BackendMask::CPU).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let opts = ModelOptions { backend: Backend::Cpu, ..ModelOptions::default() };

    let t = Instant::now();
    let vad_model = Model::load_with(&a[3], &opts).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let mut vad = vad_model.vad_session().map_err(|e| anyhow::anyhow!(e.to_string()))?;
    println!("vad load {:.0} ms", ms(t));
    let t = Instant::now();
    let segs = vad.run(&pcm, &VadOptions::default()).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    println!("vad run {:.0} ms ({} segments)", ms(t), segs.segments.len());

    let threads: Vec<i32> = if a.len() > 4 { a[4..].iter().map(|s| s.parse().unwrap()).collect() } else { vec![4] };
    let t = Instant::now();
    let model = Model::load_with(&a[2], &opts).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    println!("asr load {:.0} ms", ms(t));
    for n in threads {
        let mut s = model.session_with(&SessionOptions { n_threads: n, ..SessionOptions::default() })
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let prompt = std::env::var("BENCH_PROMPT").ok();
        let ro = RunOptions { language: Some("zh".into()), prompt: prompt.clone(), ..RunOptions::default() };
        if let Some(p) = &prompt { println!("prompt {} chars", p.chars().count()); }
        for i in 0..3 {
            let t = Instant::now();
            let r = s.run(&pcm, &ro).map_err(|e| anyhow::anyhow!(e.to_string()))?;
            println!("threads={n:2} run#{i} {:.0} ms  {}", ms(t), r.text.trim());
        }
    }
    let t = Instant::now();
    let out = voxkit::convert::maybe_convert("嘿嘿，我想测试，我想去台北吃。", "s2twp");
    println!("opencc {:.0} ms -> {out}", ms(t));
    Ok(())
}
