#!/usr/bin/env python3
"""html-hub 로컬 MCP 서버 (stdio). 표준 라이브러리만 쓴다.

도구는 원격 MCP(<HTML_HUB_URL>/mcp)의 것을 그대로 중계한다 (upload_html, update_html, list_documents ...).
여기서 더하는 것은 하나: 내 컴퓨터의 파일을 file_path 로 넘기면 읽어서 content·format 으로 바꿔 보낸다.
OAuth 없이 직원 토큰으로 붙고 싶을 때 쓴다. 원격 MCP 를 바로 등록할 수 있으면 그쪽이 더 간단하다.

환경변수: HTML_HUB_URL (예: https://survey.eunseong.dev), HTML_HUB_TOKEN (사이트의 /tokens 에서 발급)
Claude Code 등록 예: claude mcp add html-hub -e HTML_HUB_URL=... -e HTML_HUB_TOKEN=... -- python3 /경로/mcp_upload.py
"""
import json, os, sys, urllib.error, urllib.request

FORMATS = {".html": "html", ".htm": "html", ".md": "md", ".svg": "svg", ".mmd": "mermaid", ".mermaid": "mermaid", ".jsx": "jsx"}
FILE_PATH = {"type": "string", "description": "content 대신 내 컴퓨터의 파일 절대 경로 (" + " ".join(FORMATS) + ")"}


def remote(method, params):
    base, token = os.environ["HTML_HUB_URL"].rstrip("/"), os.environ["HTML_HUB_TOKEN"]
    req = urllib.request.Request(
        base + "/mcp", method="POST",
        data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode(),
        # 기본 Python User-Agent 는 Cloudflare 가 막는다 (error 1010).
        headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json", "User-Agent": "html-hub-mcp/0.2"},
    )
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            body = json.load(r)
    except urllib.error.HTTPError as e:
        raise RuntimeError("토큰이 올바르지 않거나 만료됐습니다" if e.code == 401 else f"HTTP {e.code}")
    if "error" in body:
        raise RuntimeError(body["error"].get("message", "원격 오류"))
    return body["result"]


def tools():
    found = remote("tools/list", {})["tools"]
    for t in found:
        props = t["inputSchema"].get("properties", {})
        if "content" in props:  # 내용을 받는 도구는 파일 경로도 받는다
            props["file_path"] = FILE_PATH
            t["inputSchema"]["required"] = [r for r in t["inputSchema"].get("required", []) if r != "content"]
    return found


def call(params):
    args = dict(params.get("arguments", {}))
    path = args.pop("file_path", None)
    if path:
        fmt = FORMATS.get(os.path.splitext(path)[1].lower())
        if not fmt:
            raise ValueError(f"올릴 수 있는 파일: {' '.join(FORMATS)}")
        with open(path, encoding="utf-8") as f:
            args["content"] = f.read()
        args.setdefault("format", fmt)
    return remote("tools/call", {"name": params["name"], "arguments": args})


def handle(msg):
    method = msg.get("method")
    if method == "initialize":
        return {"protocolVersion": msg["params"].get("protocolVersion", "2024-11-05"),
                "capabilities": {"tools": {}}, "serverInfo": {"name": "html-hub", "version": "0.2.0"}}
    if method == "tools/list":
        return {"tools": tools()}
    if method == "tools/call":
        try:
            return call(msg["params"])
        except Exception as e:  # 도구 오류는 결과로 돌려줘 Claude 가 읽게 한다
            return {"content": [{"type": "text", "text": f"실패: {e}"}], "isError": True}
    if method == "ping":
        return {}
    if msg.get("id") is None:  # 알림(initialized 등)은 답하지 않는다
        return None
    raise ValueError(f"지원하지 않는 메서드: {method}")


for line in sys.stdin:
    if not line.strip():
        continue
    try:
        msg = json.loads(line)
    except ValueError:  # 깨진 줄 하나로 서버가 죽지 않게 한다
        print(json.dumps({"jsonrpc": "2.0", "id": None, "error": {"code": -32700, "message": "JSON 이 아닙니다"}}), flush=True)
        continue
    try:
        result = handle(msg)
        if msg.get("id") is not None and result is not None:
            print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}), flush=True)
    except Exception as e:
        if msg.get("id") is not None:
            print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32603, "message": str(e)}}), flush=True)
