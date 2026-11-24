use lu_core::gguf::{inspect, GgufInfo};

#[test]
fn context_cache_estimate() {
    // Llama-3-8B shape: 32 layers, 8 KV heads of 128: 128 KiB per token.
    let i = GgufInfo { block_count: Some(32), head_count: Some(32), head_count_kv: Some(8), embedding_length: Some(4096), ..Default::default() };
    assert_eq!(i.kv_cache_mib(8192), Some(1024));
    // Hybrid: a cache on every 4th layer only.
    let h = GgufInfo { full_attention_interval: Some(4), ..i.clone() };
    assert_eq!(h.kv_cache_mib(8192), Some(256));
    assert_eq!(GgufInfo::default().kv_cache_mib(8192), None);
}

/// Owner PC: the estimate for the owner's own model.
#[test]
#[ignore]
fn owner_model_estimate() {
    let p = std::path::Path::new(r"C:\Users\UserMN4312\.lmstudio\models\lmstudio-community\Qwen3.8-27B-GGUF\Qwen3.8-27B-Q4_K_M.gguf");
    let i = inspect(p).unwrap();
    println!("{:?}", i);
    for ctx in [16384u64, 65536, 262144] {
        println!("ctx {ctx}: kv {:?} MiB, weights {} MiB", i.kv_cache_mib(ctx), i.file_bytes / 1048576);
    }
}
