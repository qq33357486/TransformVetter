# TransformVetter E2E 测试套件

零外部依赖(仅 bash / curl / docker / python3 标准库)的端到端验收测试,在**宿主 Linux bash** 运行,用于在部署前后验证 `deploy-harden` 分支的关键行为(长流超时、透传保真、审核链路、错误映射)。整套测试只创建 `tv-e2e` / `tv-e2e-mock` 两个一次性容器,宿主端口只绑 `127.0.0.1:18000` / `18001`,**不触碰线上任何容器**。

## 目录结构

```
scripts/e2e/
├── mock_upstream.py   # mock 上游(仅标准库,ThreadingHTTPServer, :18001)
│                      #   POST /v1/chat/completions         业务上游(非流式 JSON / 流式 SSE,时长=body.mock_dur_sec)
│                      #   POST /review/v1/chat/completions  复核模型(标记词 MOCK_REVIEW_BLOCK / MOCK_REVIEW_GARBAGE)
│                      #   GET/DELETE /__requests            请求录制(供断言 query / content-encoding / body 原样)
├── run_e2e.sh         # 主脚本:准备 /tmp/tv-e2e → 起 mock → 起被测容器 → 跑 11 个用例 → 清理
└── README.md          # 本文件
```

运行期产物在 `/tmp/tv-e2e/`(`.env`、`configs/`(含 e2e 复核 profile)、`out/`(各用例收发样本)、`logs/`)。清理时**保留 logs** 便于排查。

## 在 Linux 服务器上运行

前置:一台有 docker 的 Linux 服务器;被测镜像已在本地镜像库(如 `docker load < tv.tar` 或已 pull);仓库已 `scp` / 克隆到服务器(至少需要 `scripts/e2e/` 与 `configs/`)。

```bash
# 1) 把仓库传到服务器(示例)
scp -r TransformVetter user@gw:/data/tv-e2e-src/

# 2) 导入被测镜像(若以 tar 传输)
docker load < transformvetter-deploy-harden.tar

# 3) 运行
cd /data/tv-e2e-src
TV_IMAGE=ghcr.io/qq33357486/transformvetter:deploy-harden bash scripts/e2e/run_e2e.sh
```

脚本自带行为:

- `trap EXIT` 清理:`docker rm -f tv-e2e`、停掉 mock(宿主进程或 `tv-e2e-mock` 容器);保留 `/tmp/tv-e2e/logs`。
- 宿主无 `python3` 时自动改用 `docker run python:3.12-alpine` 承载 mock 与断言脚本(注意:该模式会把 18001 临时发布到 `0.0.0.0`,测试时长约 2 分钟,服务器应有安全组/防火墙拦截外网)。
- 预检:TV_IMAGE 存在、curl/gzip/docker 可用、18000/18001 端口空闲、仓库 `configs/` 文件齐全。
- 被测容器:`docker run -d --name tv-e2e -p 127.0.0.1:18000:8000 --add-host=host.docker.internal:host-gateway -v /tmp/tv-e2e/configs:/app/configs --env-file /tmp/tv-e2e/.env --cpus 1 --memory 768m $TV_IMAGE`,healthz 重试 30 次等就绪。
- 复核 profile(`configs/mod_profiles/e2e/`)由 default 复制改造:`ai.base_url` 指向 mock 复核端点、假 key、`timeout=10`、`max_retries=0`、阈值 0.30/0.50;**不放置任何本地模型文件**,因此所有请求都走 LLM 复核链路,且全程不做训练(`TRAINING_SCHEDULER_ENABLED=false`)。

## 用例表

| # | 名称 | 验证点 | 通过条件 |
|---|------|--------|----------|
| 1 | healthz | 服务就绪 | 200 且 `ok:true` |
| 2 | 路由配置解析 | `!ENV_KEY$URL` 语法 | `/debug/url-config` 返回 `source=env:TEST_ROUTE`、upstream、format_transform.enabled |
| 3 | 关键词拦截 | 基础层 | 含 `ignore previous instructions`(仓库词库公开技术词)→ 400,响应含 `BASIC_MODERATION_BLOCKED` |
| 4 | 正常透传 | 请求/响应保真 | 200;mock 收到的 body 与发送 JSON 语义一致;mock 的完整响应回到客户端 |
| 5 | **长流不断(核心)** | 60s 总超时修复(P0-A) | `stream:true` + `mock_dur_sec=75`:客户端完整收流,时长 ≥ 74s(留 1s 计时容差)、SSE 帧 ≥ 140、以 `[DONE]` 结束、curl 正常退出 |
| 6 | query 透传 | 上游 URL query 保留(P1) | 路由 URL 带 `%3Falpha=1%26beta=2`,mock 录制到 `query == "alpha=1&beta=2"` |
| 7 | 压缩请求 | gzip 解压 + Content-Encoding 剔除(P1) | `curl --compressed` + `Content-Encoding: gzip` 发送,mock 收到可解析 JSON 且请求头**无** content-encoding |
| 8 | 智能复核拦截 | 复核链路生效 | prompt 含 `MOCK_REVIEW_BLOCK` → 400 `MODERATION_BLOCKED` |
| 9 | 复核解析失败不放行 | fail-closed | prompt 含 `MOCK_REVIEW_GARBAGE` → 重试耗尽后 HTTP ≥ 400(不允许 200 静默放行) |
| 10 | 上游拒绝映射 | 500→502(P1) | ENV_KEY 指向本机未监听端口 → 客户端收到 **502**(不是 500) |
| 11 | 性能冒烟 | 仅采集 | 串行 20 次非流式请求,打印 avg/P95 延迟,不断言阈值 |

用例 5/6/7/10 是 `DEPLOY.md` 修复清单(P0-A 长流超时、query 丢弃、Content-Encoding 转发、上游失败 500)的验收项:**在未包含对应修复的镜像上,这些用例会按预期 FAIL**——这正是指示待修复/回归的方式,不是测试脚本误报。

## 输出格式

- 每个用例一行:`PASS: <名称>` 或 `FAIL: <名称> <原因>`;采集类信息以 `INFO:` 前缀输出。
- 最后一行:`SUMMARY: passed=N failed=M`。
- 进程退出码 = 失败用例数(全部通过为 0),可直接接入 CI / 部署门禁。
- 事后排查:`/tmp/tv-e2e/logs/` 下有 `tv-container.log`(被测容器 stdout)与 `mock.log`;`/tmp/tv-e2e/out/` 下有各用例的请求/响应原文(case5 的完整 SSE 流在 `case5.sse`)。

## 安全约束

- 测试样本只使用公开技术术语与占位标记(如提示注入研究通用词 `ignore previous instructions`、`MOCK_REVIEW_BLOCK`),不编写真实违禁内容。
- mock 与被测容器都只做测试;被测容器端口仅绑 `127.0.0.1`;`DEBUG=true` 的 debug 路由不暴露公网。
