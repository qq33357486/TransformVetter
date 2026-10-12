#!/usr/bin/env python3
"""把 tv-research 标定产物导出为 TransformVetter 可用的 oneclass 资产。

产物(分数空间与 src/moderation/oneclass.rs 逐位对齐):
  1. <profile_dir>/oneclass_model.json —— 词表模式 v2 模型:
     vocab(sklearn CountVectorizer.vocabulary_ 的逆序数组)+ 稀疏质心
     (L2 归一,来自 cache_decision_sim/centroid.npy)。
     Rust 侧 TF·质心相似度与 decision_sim.score_all 同式,OPERATING_POINT
     的阈值可原样填入 profile。
  2. <profile_dir>/profile.json —— 合并 "oneclass" 段:阈值/采样率/复核
     置信线/解析失败策略,全部取自 OPERATING_POINT.json 的运营点
     (block=2.0 之类的保守默认值只做缺省,导出即标定值)。
  3. tests/fixtures/oneclass_parity.json —— 对数夹具(决策模拟每样本留底
     decision_sim_samples.jsonl 的兑现):分层抽 N 条真实样本 + Python 侧
     异常分,Rust 对数测试(tests/oneclass_parity_tests.rs)逐条复算比对。

用法:
  python scripts/export_oneclass_model.py \
      --research-dir C:/Users/limen/git/tv-research \
      --profile-dir configs/mod_profiles/default \
      --fixture-out tests/fixtures/oneclass_parity.json

前置:research 仓库先跑过 scripts/decision_sim.py(cache_decision_sim/
下有 vec.pkl / centroid.npy / scores_v2.npz / stage1.pkl)。
"""
from __future__ import annotations

import argparse
import json
import pickle
import sys
from pathlib import Path

import numpy as np

DEFAULT_RESEARCH = Path(r"C:\Users\limen\git\tv-research")
MAX_CHARS = 100_000  # 与 decision_sim.MAX_CHARS / Rust max_scan_chars 默认一致
C_GRAY = 0.80        # decision_sim.C_GRAY(OP.json 未单列,取脚本常量)
NONE_POLICY_MAP = {"self": "self", "fallthrough": "fallthrough", "failclosed": "block"}


def load_point(research_dir: Path, variant: str) -> dict:
    op = json.loads((research_dir / "data" / "OPERATING_POINT.json").read_text(encoding="utf-8"))
    point = op.get(variant)
    if not point:
        raise SystemExit(f"OPERATING_POINT.json 缺少 {variant}")
    return point


def build_model(vec, centroid: np.ndarray) -> dict:
    vocabulary = vec.vocabulary_  # {gram: idx}
    vocab_size = len(vocabulary)
    vocab = [None] * vocab_size
    for gram, idx in vocabulary.items():
        vocab[idx] = gram
    if any(g is None for g in vocab):
        raise SystemExit("vocabulary_ 下标不连续,无法导出词表数组")

    centroid = np.asarray(centroid).ravel()
    if centroid.shape[0] != vocab_size:
        raise SystemExit(f"centroid 维度 {centroid.shape[0]} != vocab {vocab_size}")
    norm = float(np.linalg.norm(centroid))
    if norm <= 0:
        raise SystemExit("centroid 范数为 0")
    centroid = centroid / norm

    sparse = {str(i): float(w) for i, w in enumerate(centroid) if w != 0.0}
    if not sparse:
        raise SystemExit("centroid 全零")
    return {
        "runtime_version": 2,
        "window_chars": 8000,
        "ngram_range": [2, 4],
        "lowercase": True,
        "vocab": vocab,
        "centroid": sparse,
    }


def build_oneclass_section(point: dict) -> dict:
    params = point["params"]
    policy = NONE_POLICY_MAP.get(params.get("none_policy", "self"), "self")
    return {
        # OPERATING_POINT 阈值原样落地(anomaly 取向:>block 拦,>divert 存疑)
        "block_threshold": float(params["oneclass_block_thr"]),
        "divert_threshold": float(params["oneclass_divert_thr"]),
        "review_base_rate": float(params["glm_sample_rate"]),
        "review_forced_model_prefixes": [],
        "review_violation_block_conf": float(params.get("glm_conf_block", 0.85)),
        "review_cleared_gray_conf": C_GRAY,
        "review_unparseable_policy": policy,
        # 标定空间不做基线归一(阈值即原始 anomaly 空间);留空数组保持 raw
        "baselines": [],
    }


def build_fixture(stage1: dict, eval_scores: np.ndarray, n: int) -> dict:
    texts, labels = stage1["texts"], stage1["labels"]
    if len(texts) != len(eval_scores):
        raise SystemExit("texts 与 eval_scores 长度不一致")
    pure_normal = [
        i for i, lab in enumerate(labels)
        if lab == "normal"
    ]
    # 分层抽样:normal 按异常分排序取头/中/尾,其余类各取头若干
    ordered = sorted(pure_normal, key=lambda i: eval_scores[i])
    picks: list[int] = []
    per_side = max(n // 4, 3)
    picks += ordered[:per_side] + ordered[len(ordered) // 2: len(ordered) // 2 + per_side] + ordered[-per_side:]
    other = [i for i, lab in enumerate(labels) if lab != "normal"]
    by_label: dict[str, list[int]] = {}
    for i in other:
        by_label.setdefault(labels[i], []).append(i)
    per_label = max((n - len(picks)) // max(len(by_label), 1), 1)
    for lab in sorted(by_label):
        picks += by_label[lab][:per_label]

    samples = []
    for i in picks:
        text = texts[i][:MAX_CHARS]  # 与 Rust max_scan_chars=100K 等价截断
        samples.append({
            "i": i,
            "label": labels[i],
            "anomaly_expected": float(eval_scores[i]),
            "text": text,
        })
    return {
        "source": "tv-research decision_sim (scores_v2 + stage1)",
        "model": "configs/mod_profiles/default/oneclass_model.json",
        "max_scan_chars": MAX_CHARS,
        "tolerance": 1e-5,
        "samples": samples,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--research-dir", type=Path, default=DEFAULT_RESEARCH)
    parser.add_argument("--profile-dir", type=Path, default=Path("configs/mod_profiles/default"))
    parser.add_argument("--variant", default="operating_point",
                        choices=["operating_point", "launch_safe_variant"])
    parser.add_argument("--fixture-out", type=Path, default=Path("tests/fixtures/oneclass_parity.json"))
    parser.add_argument("--fixture-n", type=int, default=40, help="对数夹具样本数(分层)")
    parser.add_argument("--skip-fixture", action="store_true")
    args = parser.parse_args()

    cache = args.research_dir / "data" / "cache_decision_sim"
    vec_pkl, cen_npy = cache / "vec.pkl", cache / "centroid.npy"
    for required in (vec_pkl, cen_npy):
        if not required.exists():
            raise SystemExit(f"缺 {required}:请先在 research 仓库跑 scripts/decision_sim.py")
    vec = pickle.loads(vec_pkl.read_bytes())
    centroid = np.load(cen_npy)

    model = build_model(vec, centroid)
    point = load_point(args.research_dir, args.variant)

    args.profile_dir.mkdir(parents=True, exist_ok=True)
    model_path = args.profile_dir / "oneclass_model.json"
    model_path.write_text(json.dumps(model, ensure_ascii=False), encoding="utf-8")
    print(f"model  -> {model_path} (vocab={len(model['vocab'])}, "
          f"centroid_nz={len(model['centroid'])}, {model_path.stat().st_size} bytes)")

    profile_path = args.profile_dir / "profile.json"
    profile = json.loads(profile_path.read_text(encoding="utf-8")) if profile_path.exists() else {}
    profile["oneclass"] = build_oneclass_section(point)
    profile_path.write_text(json.dumps(profile, ensure_ascii=False, indent=2), encoding="utf-8")
    print(f"profile-> {profile_path} (oneclass 段 = {args.variant} 标定值)")
    print(json.dumps(profile["oneclass"], ensure_ascii=False, indent=2))

    if not args.skip_fixture:
        stage1_pkl = cache / "stage1.pkl"
        scores_npz = cache / "scores_v2.npz"
        for required in (stage1_pkl, scores_npz):
            if not required.exists():
                raise SystemExit(f"缺 {required}:请先在 research 仓库跑 scripts/decision_sim.py")
        stage1 = pickle.loads(stage1_pkl.read_bytes())
        eval_scores = np.load(scores_npz)["eval"]
        fixture = build_fixture(stage1, eval_scores, args.fixture_n)
        args.fixture_out.parent.mkdir(parents=True, exist_ok=True)
        args.fixture_out.write_text(json.dumps(fixture, ensure_ascii=False), encoding="utf-8")
        print(f"fixture-> {args.fixture_out} ({len(fixture['samples'])} samples)")


if __name__ == "__main__":
    sys.exit(main())
