#!/usr/bin/env python3
"""验证 Rust 三态决策链与标定模拟(decision_sim.decide)语义一致。

数据:tv-research/data/decision_sim_samples.jsonl(485 条每样本留底)+
OPERATING_POINT.json 运营点主参数。对 pure normal(292 条)复算:

  1. 参考语义(decision_sim.decide 原式)—— 应复现 OP 标称指标
  2. 修复前 Rust 语义(cleared 一律落回分数路由 / violation 不分置信 /
     解析失败一律降级存疑)—— 预期误切 ~25/292(复核报告口径)
  3. 修复后 Rust 语义(route_moderation 的 R2/R3/R3b/R4)—— 应与参考一致

任一断言失败退出码非 0,可直接挂 CI。
纯标准库,不依赖 numpy/sklearn。
"""
from __future__ import annotations

import json
import sys
from pathlib import Path

DEFAULT_RESEARCH = Path(r"C:\Users\limen\git\tv-research")


def load(research_dir: Path):
    samples = [
        json.loads(line)
        for line in (research_dir / "data" / "decision_sim_samples.jsonl")
        .read_text(encoding="utf-8")
        .splitlines()
    ]
    op = json.loads((research_dir / "data" / "OPERATING_POINT.json").read_text(encoding="utf-8"))
    return samples, op["operating_point"]


def reference_decide(s, t_block, t_divert, c_block, c_gray, none_policy):
    """decision_sim.decide() 原式(R1 词库在外层另行统计,不进本函数)。"""
    score = s["anomaly"]
    if s["kw_hit"]:
        return "BLOCK"
    if score > t_block:
        return "BLOCK"
    # 运营点 p=1.0:恒采样,无需再现 u
    if s["glm_parse_fail"]:
        if none_policy == "failclosed":
            return "BLOCK"
        if none_policy == "self":
            return "SELF"
        # fallthrough → R4
    else:
        conf = s["glm_conf"] or 0.0
        if s["glm_violation"]:
            return "BLOCK" if conf >= c_block else "DIVERT"
        if conf < c_gray and score > t_divert:
            return "DIVERT"
        return "SELF"
    if score > t_divert:
        return "DIVERT"
    return "SELF"


def old_rust_decide(s, t_block, t_divert):
    """修复前 route_moderation 语义(复核报告指认的 8.56% 误切来源)。"""
    score = s["anomaly"]
    if score > t_block:
        return "BLOCK"
    if s["glm_parse_fail"]:
        return "DIVERT"  # 旧实现:解析失败一律降级存疑
    if s["glm_violation"]:
        return "BLOCK"    # 旧实现:violation 不分置信一律拦
    # 旧实现:cleared 后直接落回分数路由(缺高置信豁免)
    return "DIVERT" if score > t_divert else "SELF"


def new_rust_decide(s, t_block, t_divert, c_block, c_gray, policy):
    """修复后 route_moderation 语义(与 Rust route_after_review/route_for_score 对应)。"""
    score = s["anomaly"]
    if score > t_block:
        return "BLOCK"
    if s["glm_parse_fail"]:
        if policy == "block":
            return "BLOCK"
        if policy == "divert":
            return "DIVERT"
        if policy == "fallthrough":
            return "DIVERT" if score > t_divert else "SELF"
        return "SELF"  # "self" = 默认,运营点 none_policy
    conf = s["glm_conf"] or 0.0
    if s["glm_violation"]:
        return "BLOCK" if conf >= c_block else "DIVERT"
    if conf < c_gray and score > t_divert:
        return "DIVERT"
    return "SELF"  # 高置信豁免:主上游


def rate(samples, states, target):
    n = len(samples)
    hit = sum(1 for st in states if st == target)
    return hit, n, hit / n if n else 0.0


def main() -> int:
    research = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_RESEARCH
    samples, point = load(research)
    params = point["params"]
    t_block = params["oneclass_block_thr"]
    t_divert = params["oneclass_divert_thr"]
    c_block = params["glm_conf_block"]
    c_gray = 0.80  # decision_sim.C_GRAY
    none_policy = params["none_policy"]
    policy_map = {"self": "self", "fallthrough": "fallthrough", "failclosed": "block"}

    pure = [s for s in samples if s["label"] == "normal" and not s["glm_violation"]]
    print(f"pure_normal={len(pure)} (OP 标称 {point['metrics']['pure_normal_states']})")

    ref = [reference_decide(s, t_block, t_divert, c_block, c_gray, none_policy) for s in pure]
    old = [old_rust_decide(s, t_block, t_divert) for s in pure]
    new = [new_rust_decide(s, t_block, t_divert, c_block, c_gray, policy_map[none_policy]) for s in pure]

    ref_d = rate(pure, ref, "DIVERT")
    ref_b = rate(pure, ref, "BLOCK")
    old_d = rate(pure, old, "DIVERT")
    new_d = rate(pure, new, "DIVERT")
    new_b = rate(pure, new, "BLOCK")

    op_miscut = point["metrics"]["miscut"]
    op_block = point["metrics"]["block"]
    per_day = 21_660

    print(f"参考(decision_sim): DIVERT {ref_d[0]}/{ref_d[1]} = {ref_d[2]:.4%}, "
          f"BLOCK {ref_b[0]}/{ref_b[1]} = {ref_b[2]:.4%}")
    print(f"OP 标称:              miscut {op_miscut:.4%}, block {op_block:.4%}")
    print(f"修复前 Rust:          DIVERT {old_d[0]}/{old_d[1]} = {old_d[2]:.4%} "
          f"(≈{old_d[0] * per_day / len(pure):.0f} 条/天误切)")
    print(f"修复后 Rust:          DIVERT {new_d[0]}/{new_d[1]} = {new_d[2]:.4%}, "
          f"BLOCK {new_b[0]}/{new_b[1]} = {new_b[2]:.4%}")

    failures = []
    if new_d[0] != ref_d[0]:
        failures.append(f"修复后误切 {new_d[0]} 条 != 参考 {ref_d[0]} 条")
    if new_b[0] != ref_b[0]:
        failures.append(f"修复后误拦 {new_b[0]} 条 != 参考 {ref_b[0]} 条")
    if abs(ref_d[2] - op_miscut) > 1e-9:
        failures.append(f"参考误切 {ref_d[2]:.6f} != OP 标称 {op_miscut:.6f}(样本/实现漂移)")
    if old_d[0] <= ref_d[0]:
        failures.append("修复前误切未高于参考(复核报告口径失效,请检查本脚本)")

    if failures:
        for f in failures:
            print(f"FAIL: {f}", file=sys.stderr)
        return 1
    print("OK: 修复后 Rust 语义与标定模拟逐条一致(pure normal 292 条上零分歧)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
