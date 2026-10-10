# TransformVetter 部署笔记(duojie fork)

上游:CassiopeiaCode/TransformVetter(原名 PrismGuard)
分支:deploy-harden
用途:New API 渠道层前置审核代理(破限 / 违法 / 蒸馏防护),保护官逆 Codex/Claude 账号池。

## 架构

```
用户 → New API(鉴权/计费)→ TransformVetter(审核,本机 :8000)→ CPA / Claude 上游
```

- 只挂需要保护的渠道(官逆),New API 渠道 base_url 改为:
  `http://127.0.0.1:8000/!ENV_KEY$真实上游地址`
- `format_transform.to` 用 `pass_through`(不转协议只审核)
- 服务只绑 127.0.0.1 / docker 内网,**不暴露公网**(debug 路由可读环境变量)

## .env 骨架(每渠道一个 ENV_KEY)

```
PORT=8000
HOST=0.0.0.0            # 容器内;docker -p 只绑内网/127.0.0.1
UPSTREAM_HTTP_TIMEOUT_SECS=300
MODERATION_API_KEY=sk-xxx   # 复核用,指向自家 New API 的令牌
TRAINING_SCHEDULER_ENABLED=true
TRAINING_SCHEDULER_INTERVAL_MINUTES=30

CODEX_OFFICIAL='{"format_transform":{"enabled":true,"strict_parse":false,"from":"openai_responses","to":"pass_through","delay_stream_header":false},"basic_moderation":{"enabled":true,"keywords_file":"configs/keywords.txt","error_code":"BASIC_MODERATION_BLOCKED"},"smart_moderation":{"enabled":true,"profile":"default"}}'
CLAUDE_OFFICIAL='{"format_transform":{"enabled":true,"strict_parse":false,"from":"claude_chat","to":"pass_through","delay_stream_header":false},"basic_moderation":{"enabled":true,"keywords_file":"configs/keywords.txt","error_code":"BASIC_MODERATION_BLOCKED"},"smart_moderation":{"enabled":true,"profile":"default"}}'
```

profile.json 的 `ai.base_url` 指自家 New API(如 http://<newapi容器>:3000/v1),
`ai.model` 填自家便宜 flash 模型;复核请求会占该令牌用量,建议专用令牌+专用模型名便于区分统计。

## 部署

```bash
docker run -d --name transformvetter \
  -p 127.0.0.1:8000:8000 --env-file /data/tv/.env \
  -v /data/tv/configs:/app/configs \
  -v /data/tv/logs:/app/logs \
  --tmpfs /app/run \
  --cpus=2 --memory=1g --restart unless-stopped \
  ghcr.io/qq33357486/transformvetter:<tag>
```

镜像构建:fork 后 GitHub Actions 的 ghcr.yml 自动出镜像(需先完成下方 P0-C 改造)。

## 审计结论与修复清单(2026-10 代码审计)

### P0——不修不能上生产

- [ ] **P0-A 60s 总超时掐断长流**(main.rs / reqwest Client `.timeout()`):
      Codex/Claude 长思考 SSE 超过 60s 必断流。改为 `connect_timeout(10s)` + `read_timeout(300s)`,
      去掉覆盖响应体全程的总超时。
- [ ] **P0-B 容器内无 systemd-run,自训练断**(scheduler.rs `build_training_subprocess_command`
      硬编码 systemd-run):加探测 fallback,不存在时直接 spawn `current_exe train-profile <name>`
      (子进程继承主进程 nice=19,天然低优先级,容器侧再用 --cpus 限制)。
- [ ] **P0-C Dockerfile / CI**:Dockerfile 加 `--features storage-debug`(自训练+存储解锁);
      ghcr.yml 改 IMAGE_NAME 为 qq33357486 命名空间、触发分支加 deploy-harden。
- [ ] **P0-D debug 路由信息泄露**:`/debug/proxy-config/:key` 可枚举环境变量。
      部署层面只绑内网(必须);代码层面可加 DEBUG=false 时不注册 debug 路由。

### P1——强烈建议(影响正确性/成本)

- [ ] LLM 复核 JSON 里 `violation` 字段缺失时默认放行(fail-open)→ 应视为解析失败触发重试
- [ ] pass_through 请求体仍被 JSON 重序列化(键序重排+CPU 浪费)→ passthrough 时直接转发原字节,
      并转发前剔除请求 `Content-Encoding` 头(否则解压后重序列化的 body 携带旧压缩头,上游解压失败)
- [ ] 上游 URL 的 query string 在路由解析中被丢弃(`Url::path()` 不含 query)→ build_upstream_url 拼回
- [ ] 流预检失败返回 429(语义错误,应为 502;New API 会误判限流)
- [ ] 上游连接失败映射 500 → 应为 502/504,便于 New API 侧区分
- [ ] 主进程整体 nice=19 会拖慢线上转发 → 只对训练子进程降优先级,server 进程不降
- [ ] 请求 body 无大小上限 → axum DefaultBodyLimit(50MB)

### P2——后续优化

- gemini 格式无提取分支(本部署不用 gemini,可忽略)
- 关键词文件不存在时静默空表(无告警)
- 缓存无 TTL;ai_prompt.txt 每次 LLM 调用读盘;模型运行时签名每请求多次 stat
- tool 内容(tool_result)不参与审核提取
- local_model_exists 检查 pkl 与 predict 检查 runtime 文件不一致
- 重复文本不查重入库,样本库会膨胀(有 max_db_items 清理兜底)

## 资源评估(4C/7.5G 部署机)

- 代理本体:Rust 单二进制,转发开销 ≈ nginx 量级
- 本地模型打分:hashlinear 查表,亚毫秒,模型文件 4MB
- 训练:单核几十秒、内存 <100MB、每 12h 一次(容器 --cpus 限制下影响更小)
- RocksDB 历史:max_db_items=100k 时稳态约 100~300MB,给 1G volume 余量

## 词库/提示词维护

- `configs/keywords.txt`:只放高置信结构特征词,热刷新;误伤词移除交给智能层
- `configs/mod_profiles/default/ai_prompt.txt`:四类判定(破限/违法/色情/蒸馏)+ 降误伤条款
- `configs/mod_profiles/default/profile.json`:复核模型/阈值/训练参数
- 生产在服务器 volume 上热改,定期把有效变更回合到本仓库
