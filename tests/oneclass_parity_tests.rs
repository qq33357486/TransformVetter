//! oneclass 对数验证:用 tv-research 标定实验的每样本留底(decision_sim
//! samples)复算 Rust 侧异常分,与 Python(sklearn CountVectorizer + TF·质心)
//! 分数逐条比对。这是 OPERATING_POINT 阈值能原样落地的分数空间凭证。
//!
//! 夹具由 scripts/export_oneclass_model.py 生成(tests/fixtures/
//! oneclass_parity.json + configs/mod_profiles/default/oneclass_model.json)。

#[path = "../src/profile.rs"]
mod profile;

#[path = "../src/moderation/mod.rs"]
mod moderation;

use profile::ModerationProfile;
use serde_json::Value;

fn fixture() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/oneclass_parity.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read {}: {err}(先跑 scripts/export_oneclass_model.py)", path.display()));
    serde_json::from_str(&raw).expect("parse oneclass parity fixture")
}

fn parity_profile(model_file: &str, max_scan_chars: usize) -> ModerationProfile {
    let base_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("configs/mod_profiles/default");
    let mut config = profile::ProfileConfig::default();
    config.oneclass.model_file = model_file.to_string();
    config.oneclass.max_scan_chars = max_scan_chars;
    ModerationProfile {
        profile_name: "parity".to_string(),
        base_dir,
        config,
    }
}

#[test]
fn rust_anomaly_matches_research_scores() {
    let fixture = fixture();
    let model_repo_rel = fixture["model"].as_str().expect("fixture model path");
    // fixture 存仓库相对路径;profile 的 model_file 只取文件名(相对 profile 目录)
    let model_file = std::path::Path::new(model_repo_rel)
        .file_name()
        .and_then(|name| name.to_str())
        .expect("model file name");
    let tolerance = fixture["tolerance"].as_f64().expect("fixture tolerance");
    let max_scan = fixture["max_scan_chars"].as_u64().expect("fixture max_scan_chars") as usize;
    let prof = parity_profile(model_file, max_scan);

    let samples = fixture["samples"].as_array().expect("fixture samples");
    assert!(
        samples.len() >= 20,
        "对数夹具样本过少({} 条),重新导出", 
        samples.len()
    );

    let mut worst: f64 = 0.0;
    let mut worst_index: i64 = -1;
    for sample in samples {
        let index = sample["i"].as_i64().expect("sample i");
        let text = sample["text"].as_str().expect("sample text");
        let expected = sample["anomaly_expected"].as_f64().expect("sample anomaly_expected");

        let score = moderation::oneclass::score_request(text, "claude-sonnet-4-5", &prof)
            .expect("score_request")
            .unwrap_or_else(|| panic!("sample {index}: 模型加载失败"));
        let diff = (score.anomaly - expected).abs();
        if diff > worst {
            worst = diff;
            worst_index = index;
        }
        assert!(
            diff <= tolerance,
            "sample {index} (label={}): Rust anomaly {} vs 研究 {} (|Δ|={diff} > {tolerance})",
            sample["label"],
            score.anomaly,
            expected
        );
    }
    // 全体通过后给出最差项,便于日后漂移排查
    eprintln!(
        "oneclass parity: {} samples, worst |Δ|={worst:.3e} (sample {worst_index})",
        samples.len()
    );
}

#[test]
fn parity_fixture_model_is_the_exported_one() {
    let fixture = fixture();
    let model_rel = fixture["model"].as_str().expect("fixture model path");
    let model_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(model_rel);
    assert!(
        model_path.exists(),
        "对数夹具引用的模型不存在: {}",
        model_path.display()
    );
}
