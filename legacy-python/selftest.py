# -*- coding: utf-8 -*-
"""端到端自检：非流式 + 流式 + 多模型可用性。"""
import json
import sys
import urllib.request

BASE = "http://127.0.0.1:8787/v1"


def _post(payload, timeout=180):
    req = urllib.request.Request(
        BASE + "/chat/completions",
        data=json.dumps(payload, ensure_ascii=False).encode("utf-8"),
        headers={"Content-Type": "application/json", "Authorization": "Bearer codearts"},
        method="POST",
    )
    return urllib.request.urlopen(req, timeout=timeout)


def test_non_stream(model):
    payload = {
        "model": model,
        "messages": [{"role": "user", "content": "只回复两个字：成功"}],
        "stream": False,
        "max_tokens": 64,
    }
    with _post(payload) as r:
        data = json.loads(r.read())
    msg = data["choices"][0]["message"]
    print(f"[非流式] {model}: finish={data['choices'][0].get('finish_reason')} "
          f"content={msg.get('content')!r} usage={data.get('usage')}")
    return True


def test_stream(model):
    payload = {
        "model": model,
        "messages": [{"role": "user", "content": "数到三，用中文"}],
        "stream": True,
        "max_tokens": 128,
    }
    chunks = 0
    text = []
    done = False
    reasoning = []
    with _post(payload) as r:
        for raw in r:
            line = raw.decode("utf-8", "replace").strip()
            if not line.startswith("data:"):
                continue
            body = line[5:].strip()
            if body == "[DONE]":
                done = True
                continue
            try:
                chunk = json.loads(body)
            except json.JSONDecodeError:
                continue
            chunks += 1
            for c in chunk.get("choices") or []:
                d = c.get("delta") or {}
                if isinstance(d.get("content"), str):
                    text.append(d["content"])
                if isinstance(d.get("reasoning_content"), str):
                    reasoning.append(d["reasoning_content"])
    print(f"[流式] {model}: chunks={chunks} done={done} "
          f"len={len(''.join(text))} reasoning_len={len(''.join(reasoning))} "
          f"sample={''.join(text)[:80]!r}")
    return chunks > 0 and done


def main():
    models = [m for m in sys.argv[1:]] or ["glm-5.3-flash"]
    ok = True
    for model in models:
        for fn in (test_non_stream, test_stream):
            try:
                ok = fn(model) and ok
            except Exception as e:  # noqa: BLE001
                body = ""
                if hasattr(e, "read"):
                    try:
                        body = e.read().decode("utf-8", "replace")[:400]
                    except Exception:
                        pass
                print(f"[失败] {model} {fn.__name__}: {e} {body}")
                ok = False
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
