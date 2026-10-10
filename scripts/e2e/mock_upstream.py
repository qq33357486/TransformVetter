#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""TransformVetter E2E mock upstream(仅 Python 标准库)。

监听 0.0.0.0:18001(可用环境变量 MOCK_PORT 覆盖),提供:

  POST /v1/chat/completions          业务上游(OpenAI 兼容)
      - body 无 stream              -> 普通 JSON chat completion
      - body stream=true            -> text/event-stream,每 0.5s 一个 delta,
                                       总时长由 body.mock_dur_sec 控制(默认 3s)
  POST /review/v1/chat/completions   复核模型端点(专供 smart_moderation 调用)
      - prompt 含 MOCK_REVIEW_BLOCK   -> {"violation": true, ...}
      - prompt 含 MOCK_REVIEW_GARBAGE -> 非 JSON 垃圾文本
      - 其他                          -> {"violation": false, ...}
      - 请求 stream=true 时按 SSE 返回(TransformVetter 复核请求固定带 stream:true)
  GET    /__requests                 返回已记录请求的 JSON 数组
  DELETE /__requests                 清空记录
  GET    /healthz                    探活

每个收到的请求记录 method/path/query/headers(全量小写,含 content-encoding、
content-type)/body 前 2KB(base64 保真 + utf-8 预览),供断言 query 透传、
Content-Encoding 剔除、请求体原样。

安全说明:本 mock 仅用于测试,不含任何真实违禁内容;标记词均为测试专用占位符。
"""

import base64
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

PORT = int(os.environ.get("MOCK_PORT", "18001"))
MAX_RECORDS = 5000
BODY_SNIPPET_BYTES = 2048
SSE_INTERVAL = 0.5

MOCK_REPLY_TEXT = "mock non-stream reply"

_records = []
_lock = threading.Lock()
_rec_seq = 0


def record_request(method, path, query, header_items, body):
    """把一个请求追加进内存记录,返回该条记录(dict,可继续补充字段)。"""
    global _rec_seq
    with _lock:
        _rec_seq += 1
        rec = {
            "id": _rec_seq,
            "ts": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
            "method": method,
            "path": path,
            "query": query or "",
            "headers": {name.lower(): value for name, value in header_items},
            "body_len": len(body),
            "body_b64": base64.b64encode(body[:BODY_SNIPPET_BYTES]).decode("ascii"),
            "body_preview": body[:BODY_SNIPPET_BYTES].decode("utf-8", "replace"),
            "stream_completed": None,
        }
        _records.append(rec)
        if len(_records) > MAX_RECORDS:
            del _records[: len(_records) - MAX_RECORDS]
        return rec


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "tv-e2e-mock/1.0"
    disable_nagle_algorithm = True

    def log_message(self, fmt, *args):
        sys.stdout.write("[mock] %s %s\n" % (self.log_date_time_string(), fmt % args))
        sys.stdout.flush()

    # ---------- 基础工具 ----------

    def _read_body(self):
        try:
            length = int(self.headers.get("Content-Length") or 0)
        except ValueError:
            length = 0
        if length <= 0:
            return b""
        return self.rfile.read(length)

    def _parse_json_obj(self, raw):
        if not raw:
            return {}
        try:
            value = json.loads(raw.decode("utf-8", "replace"))
        except Exception:
            return {}
        return value if isinstance(value, dict) else {}

    def _send_json(self, obj, status=200):
        data = json.dumps(obj).encode("utf-8")
        try:
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError, OSError):
            self.close_connection = True

    def _send_sse(self, frames, per_frame_delay=0.0):
        """逐帧写 SSE。返回 True=全部写完,False=对端提前断开。"""
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        completed = True
        try:
            for frame in frames:
                self.wfile.write(frame.encode("utf-8"))
                self.wfile.flush()
                if per_frame_delay > 0:
                    time.sleep(per_frame_delay)
        except (BrokenPipeError, ConnectionResetError, OSError):
            completed = False
        finally:
            self.close_connection = True
        return completed

    # ---------- 路由 ----------

    def do_POST(self):
        raw = self._read_body()
        parsed = urlsplit(self.path)
        path, query = parsed.path, parsed.query
        rec = record_request("POST", path, query, self.headers.items(), raw)

        if path == "/v1/chat/completions":
            self._handle_business(self._parse_json_obj(raw), rec)
        elif path == "/review/v1/chat/completions":
            self._handle_review(self._parse_json_obj(raw))
        else:
            self._send_json({"error": {"message": "not found", "code": "MOCK_NOT_FOUND"}}, 404)

    def do_GET(self):
        parsed = urlsplit(self.path)
        path = parsed.path
        if path == "/__requests":
            with _lock:
                snapshot = list(_records)
            self._send_json({"count": len(snapshot), "requests": snapshot})
        elif path == "/healthz":
            self._send_json({"ok": True, "service": "tv-e2e-mock"})
        else:
            self._send_json({"error": {"message": "not found", "code": "MOCK_NOT_FOUND"}}, 404)

    def do_DELETE(self):
        parsed = urlsplit(self.path)
        if parsed.path == "/__requests":
            with _lock:
                _records.clear()
            self._send_json({"ok": True, "cleared": True})
        else:
            self._send_json({"error": {"message": "not found", "code": "MOCK_NOT_FOUND"}}, 404)

    # ---------- 业务上游 ----------

    def _handle_business(self, body, rec):
        model = body.get("model") or "mock-model"

        if body.get("stream"):
            try:
                dur = float(body.get("mock_dur_sec", 3))
            except (TypeError, ValueError):
                dur = 3.0
            dur = max(dur, SSE_INTERVAL)
            n_chunks = int(round(dur / SSE_INTERVAL))

            def frames():
                for i in range(n_chunks):
                    chunk = {
                        "id": "chatcmpl-e2e-stream",
                        "object": "chat.completion.chunk",
                        "model": model,
                        "choices": [
                            {
                                "index": 0,
                                "delta": {"content": "tick-%d " % i},
                                "finish_reason": None,
                            }
                        ],
                    }
                    yield "data: %s\n\n" % json.dumps(chunk, separators=(",", ":"))
                yield "data: [DONE]\n\n"

            completed = self._send_sse(frames(), SSE_INTERVAL)
            rec["stream_completed"] = completed
            rec["stream_dur_sec"] = dur
            return

        self._send_json(
            {
                "id": "chatcmpl-e2e-mock",
                "object": "chat.completion",
                "model": model,
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": MOCK_REPLY_TEXT},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
            }
        )

    # ---------- 复核模型 ----------

    def _handle_review(self, body):
        content = ""
        messages = body.get("messages")
        if isinstance(messages, list) and messages and isinstance(messages[0], dict):
            value = messages[0].get("content")
            if isinstance(value, str):
                content = value
            elif isinstance(value, list):
                content = " ".join(
                    part.get("text", "") for part in value if isinstance(part, dict)
                )

        if "MOCK_REVIEW_BLOCK" in content:
            verdict_text = json.dumps(
                {
                    "violation": True,
                    "category": "jailbreak",
                    "reason": "mock review block marker hit",
                    "confidence": 0.99,
                }
            )
        elif "MOCK_REVIEW_GARBAGE" in content:
            verdict_text = "GARBAGE-VERDICT not-json-text without braces +++"
        else:
            verdict_text = json.dumps(
                {
                    "violation": False,
                    "category": "none",
                    "reason": "mock review allow",
                    "confidence": 0.05,
                }
            )

        if body.get("stream"):
            chunk = {
                "id": "chatcmpl-mock-review",
                "object": "chat.completion.chunk",
                "choices": [
                    {"index": 0, "delta": {"content": verdict_text}, "finish_reason": None}
                ],
            }

            def frames():
                yield "data: %s\n\n" % json.dumps(chunk)
                yield "data: [DONE]\n\n"

            self._send_sse(frames(), 0.0)
        else:
            self._send_json(
                {
                    "id": "chatcmpl-mock-review",
                    "object": "chat.completion",
                    "model": body.get("model") or "mock-review-model",
                    "choices": [
                        {
                            "index": 0,
                            "message": {"role": "assistant", "content": verdict_text},
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
                }
            )


def main():
    server = ThreadingHTTPServer(("0.0.0.0", PORT), Handler)
    server.daemon_threads = True
    print("[mock] listening on 0.0.0.0:%d" % PORT, flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
