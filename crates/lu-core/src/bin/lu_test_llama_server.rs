//! Test double for llama.cpp's `llama-server`, used only by Lu's tests.
//!
//! It answers the same command line, `/health` and `/completion` that Lu
//! uses, without a model. Behaviour is chosen by files next to the binary
//! (so one test can install several "engine builds" that act differently):
//!
//! - `FAIL_LOAD`: exit with a load error, like a backend that cannot use
//!   this GPU;
//! - `CRASH_AFTER_LOAD`: become healthy, then exit on the first completion;
//! - `SLOW_LOAD_MS` (contents: milliseconds): delay before healthy;
//! - `NO_DEVICES`: `--list-devices` shows no GPU (a GPU build that cannot
//!   see this machine's GPU);
//! - `CPU_ONLY`: loads, but reports 0 layers offloaded (llama.cpp's quiet
//!   fall back to the CPU).
//!
//! `--version` prints a version line like the real server.

use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::PathBuf,
    thread,
    time::Duration,
};

fn main() {
    let args: Vec<String> = env::args().collect();
    let here: PathBuf = env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from)).unwrap_or_default();
    if args.iter().any(|a| a == "--version") {
        println!("version: 12651 (lu-test-double)");
        println!("built with lu test for {}", env::consts::OS);
        return;
    }
    if args.iter().any(|a| a == "--list-devices") {
        println!("Available devices:");
        if !here.join("NO_DEVICES").exists() {
            println!("  TEST0: Lu Test GPU (8192 MiB, 8000 MiB free)");
        }
        return;
    }
    let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let port: u16 = value("--port").and_then(|p| p.parse().ok()).unwrap_or(0);
    let key = value("--api-key").unwrap_or_default();
    let model = value("--model").unwrap_or_default();
    if here.join("FAIL_LOAD").exists() {
        eprintln!("ggml_backend: device not usable on this machine");
        eprintln!("error: failed to load model '{model}'");
        std::process::exit(1);
    }
    if !fs::metadata(&model).map(|m| m.is_file()).unwrap_or(false) {
        eprintln!("error: failed to open model '{model}'");
        std::process::exit(1);
    }
    let crash_after_load = here.join("CRASH_AFTER_LOAD").exists();
    let layers: u32 = value("--n-gpu-layers").and_then(|v| v.parse().ok()).unwrap_or(0);
    let on_gpu = if here.join("CPU_ONLY").exists() { 0 } else { layers.min(33) };
    eprintln!("load_tensors: offloaded {on_gpu}/33 layers to GPU");
    let slow: u64 = fs::read_to_string(here.join("SLOW_LOAD_MS")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind");
    let ready_at = std::time::Instant::now() + Duration::from_millis(slow);
    eprintln!("main: server is listening on http://127.0.0.1:{port}");
    for stream in listener.incoming() {
        let Ok(mut s) = stream else { continue };
        let mut reader = BufReader::new(s.try_clone().expect("clone"));
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            continue;
        }
        let mut auth = String::new();
        let mut length = 0usize;
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                break;
            }
            let lower = h.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("authorization: ") {
                auth = v.trim().to_owned();
            }
            if let Some(v) = lower.strip_prefix("content-length: ") {
                length = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; length];
        let _ = reader.read_exact(&mut body);
        let reply = |s: &mut std::net::TcpStream, status: &str, body: &str| {
            let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        };
        let path = line.split_whitespace().nth(1).unwrap_or("/");
        if path == "/health" {
            if std::time::Instant::now() < ready_at {
                reply(&mut s, "503 Service Unavailable", r#"{"error":{"message":"Loading model"}}"#);
            } else {
                reply(&mut s, "200 OK", r#"{"status":"ok"}"#);
            }
            continue;
        }
        if auth != format!("bearer {}", key.to_ascii_lowercase()) {
            reply(&mut s, "401 Unauthorized", r#"{"error":{"message":"Invalid API Key"}}"#);
            continue;
        }
        if path == "/completion" {
            if crash_after_load {
                eprintln!("GGML_ASSERT failed: simulated crash");
                std::process::exit(134);
            }
            thread::sleep(Duration::from_millis(20));
            reply(&mut s, "200 OK", r#"{"content":" Hello from the test engine.","tokens_predicted":6,"timings":{"predicted_per_second":42.5,"prompt_per_second":310.0}}"#);
        } else {
            reply(&mut s, "404 Not Found", r#"{"error":{"message":"not found"}}"#);
        }
    }
}
