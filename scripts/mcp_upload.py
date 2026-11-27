#!/usr/bin/env python3
"""html-hub 에 HTML 을 올리는 최소 MCP 서버 (stdio). 표준 라이브러리만 쓴다.

환경변수: HTML_HUB_URL (예: https://survey.eunseong.dev), HTML_HUB_TOKEN (사이트의 /tokens 에서 발급)
Claude Code 등록 예: claude mcp add html-hub -e HTML_HUB_URL=... -e HTML_HUB_TOKEN=... -- python3 /경로/mcp_upload.py
"""
import json, os, sys, urllib.error, urllib.request, uuid

EXTS = {"html": "html", "md": "md", "svg": "svg", "mermaid": "mmd", "jsx": "jsx"}

TOOL = {
    "name": "upload_html",
    "description": "survey(html-hub, 사내 HTML 문서 공유 사이트)에 문서를 올리고 공유 링크를 돌려준다. "
                   "Claude 가 만든 아티팩트는 내용 전체를 content 에 넣고, 이미 파일로 있으면 file_path 를 준다. "
                   ".html 외에 Markdown, SVG, Mermaid, React(.jsx, export default 컴포넌트)도 받아 서버가 HTML 로 감싼다. "
                   "올린 문서는 전 직원이 볼 수 있다.",
    "inputSchema": {
        "type": "object",
        "properties": {
            "title": {"type": "string", "description": "문서 제목"},
            "content": {"type": "string", "description": "문서 내용 전체 (file_path 대신)"},
            "format": {"type": "string", "enum": list(EXTS), "description": "content 의 종류. 기본 html"},
            "file_path": {"type": "string", "description": "올릴 파일의 절대 경로 (.html .htm .md .svg .mmd .mermaid .jsx)"},
            "project": {"type": "string", "description": "프로젝트 이름 (선택)"},
        },
        "required": ["title"],
    },
}


def upload(title, project="", file_path=None, content=None, format="html"):
    base, token = os.environ["HTML_HUB_URL"].rstrip("/"), os.environ["HTML_HUB_TOKEN"]
    if content:
        if format not in EXTS:
            raise ValueError(f"format 은 {', '.join(EXTS)} 중 하나입니다")
        data, filename = content.encode(), f"artifact.{EXTS[format]}"
    elif file_path:
        with open(file_path, "rb") as f:
            data = f.read()
        filename = os.path.basename(file_path)
    else:
        raise ValueError("content 나 file_path 가 필요합니다")
    b = uuid.uuid4().hex
    body = b""
    for name, value in (("title", title), ("project", project)):
        body += f'--{b}\r\nContent-Disposition: form-data; name="{name}"\r\n\r\n{value}\r\n'.encode()
    body += f'--{b}\r\nContent-Disposition: form-data; name="file"; filename="{filename}"\r\nContent-Type: application/octet-stream\r\n\r\n'.encode()
    body += data + f"\r\n--{b}--\r\n".encode()
    req = urllib.request.Request(
        base + "/api/upload", data=body, method="POST",
        # 기본 Python User-Agent 는 Cloudflare 가 막는다 (error 1010).
        headers={"Authorization": f"Bearer {token}", "Content-Type": f"multipart/form-data; boundary={b}",
                 "User-Agent": "html-hub-mcp/0.1"},
    )
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.load(r)["url"]
    except urllib.error.HTTPError as e:
        try:
            msg = json.load(e).get("error", f"HTTP {e.code}")
        except ValueError:
            msg = f"HTTP {e.code}"
        raise RuntimeError(msg)


def handle(msg):
    method, id_ = msg.get("method"), msg.get("id")
    if method == "initialize":
        return {"protocolVersion": msg["params"].get("protocolVersion", "2024-11-05"),
                "capabilities": {"tools": {}}, "serverInfo": {"name": "html-hub", "version": "0.1.0"}}
    if method == "tools/list":
        return {"tools": [TOOL]}
    if method == "tools/call":
        a = msg["params"].get("arguments", {})
        try:
            url = upload(a["title"], a.get("project", ""), a.get("file_path"), a.get("content"), a.get("format", "html"))
            return {"content": [{"type": "text", "text": url}]}
        except Exception as e:  # 도구 오류는 결과로 돌려줘 Claude 가 읽게 한다
            return {"content": [{"type": "text", "text": f"올리기 실패: {e}"}], "isError": True}
    if method == "ping":
        return {}
    if id_ is None:  # 알림(initialized 등)은 답하지 않는다
        return None
    raise ValueError(f"지원하지 않는 메서드: {method}")


for line in sys.stdin:
    if not line.strip():
        continue
    msg = json.loads(line)
    try:
        result = handle(msg)
        if msg.get("id") is not None and result is not None:
            print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}), flush=True)
    except Exception as e:
        if msg.get("id") is not None:
            print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": str(e)}}), flush=True)
