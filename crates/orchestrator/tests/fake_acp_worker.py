#!/usr/bin/env python3
"""A fake ACP worker for tests.

Speaks ACP v1 on stdio and calls the executor's worker tools over MCP (streamable HTTP).
The brief (first prompt) selects a mode with a `MODE:<name>` marker:

  complete    write result.txt, call `complete`, end the turn
  ask         call `ask_orchestrator`, end the turn; on the answer, write it and complete
  permission  request permission; complete if allowed
  silent      end the turn without completing; on the next prompt, complete
  slow        keep running until cancelled
  followup    call `propose_followup`, then complete
  slowonce    run until cancelled the first time (leaving a marker); complete on a retry
"""
import json
import os
import sys
import threading
import time
import urllib.request

state = {"mcp": None, "session": None, "mode": None, "cwd": ".", "cancelled": False, "next_id": 1000}
pending = {}
lock = threading.Lock()


def send(obj):
    with lock:
        sys.stdout.write(json.dumps(obj) + "\n")
        sys.stdout.flush()


def say(sid, text):
    send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": sid, "update": {
        "sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}}})


def mcp_post(body, session=None):
    m = state["mcp"]
    headers = {h["name"]: h["value"] for h in m.get("headers", [])}
    headers.update({"Content-Type": "application/json", "Accept": "application/json, text/event-stream"})
    if session:
        headers["mcp-session-id"] = session
    req = urllib.request.Request(m["url"], data=json.dumps(body).encode(), headers=headers, method="POST")
    with urllib.request.urlopen(req, timeout=20) as r:
        sid = r.headers.get("mcp-session-id")
        raw = r.read().decode()
    for line in raw.splitlines():
        if line.startswith("data:"):
            data = line[5:].strip()
            if data:
                return sid, json.loads(data)
    return sid, (json.loads(raw) if raw.strip() else None)


def tool(name, args):
    if not state.get("mcp_session"):
        sid, _ = mcp_post({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "fake", "version": "0"}}})
        state["mcp_session"] = sid
        mcp_post({"jsonrpc": "2.0", "method": "notifications/initialized"}, sid)
    _, resp = mcp_post({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                        "params": {"name": name, "arguments": args}}, state["mcp_session"])
    return resp


def request(method, params):
    state["next_id"] += 1
    rid = state["next_id"]
    ev = threading.Event()
    pending[rid] = {"event": ev}
    send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
    ev.wait(30)
    return pending.pop(rid).get("result")


def write(name, text):
    with open(os.path.join(state["cwd"], name), "w") as f:
        f.write(text)


def handle_prompt(rid, params):
    sid = params["sessionId"]
    text = "".join(b.get("text", "") for b in params["prompt"])
    if state["mode"] is None:
        for m in ["complete", "ask", "permission", "silent", "slowonce", "followup", "slow"]:
            if "MODE:" + m in text and state["mode"] is None:
                state["mode"] = m
        first = True
    else:
        first = False
    mode = state["mode"]
    stop = "end_turn"
    say(sid, "Working on it. ")
    if mode == "complete":
        write("result.txt", "done\n")
        tool("complete", {"summary": "Wrote result.txt", "evidence": "cat result.txt"})
    elif mode == "ask":
        if first:
            tool("ask_orchestrator", {"question": "Which database should I use?"})
        else:
            write("answer.txt", text + "\n")
            tool("complete", {"summary": "Used the answer: " + text})
    elif mode == "permission":
        r = request("session/request_permission", {"sessionId": sid, "toolCall": {"toolCallId": "t1", "title": "git push"},
            "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                        {"optionId": "deny", "name": "Deny", "kind": "reject_once"}]})
        outcome = (r or {}).get("outcome", {})
        if outcome.get("optionId") == "allow":
            write("pushed.txt", "yes\n")
            tool("complete", {"summary": "Pushed"})
        else:
            say(sid, "Not allowed.")
    elif mode == "silent":
        if not first:
            tool("complete", {"summary": "Finished after a nudge"})
    elif mode == "followup":
        tool("propose_followup", {"title": "Add more tests", "why": "coverage is thin"})
        tool("complete", {"summary": "Done, with a follow-up"})
    elif mode == "slowonce":
        marker = os.path.join(state["cwd"], ".slowonce")
        if os.path.exists(marker):
            write("retried.txt", "ok\n")
            tool("complete", {"summary": "Completed on retry"})
        else:
            write(".slowonce", "1\n")
            while not state["cancelled"]:
                time.sleep(0.05)
            stop = "cancelled"
    elif mode == "slow":
        while not state["cancelled"]:
            time.sleep(0.05)
        stop = "cancelled"
    send({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": stop}})


def main():
    for line in sys.stdin:
        msg = json.loads(line)
        method, rid, params = msg.get("method"), msg.get("id"), msg.get("params", {})
        if method is None and rid in pending:
            pending[rid]["result"] = msg.get("result")
            pending[rid]["event"].set()
        elif method == "initialize":
            send({"jsonrpc": "2.0", "id": rid, "result": {"protocolVersion": 1, "agentCapabilities": {
                "mcpCapabilities": {"http": True}}, "authMethods": []}})
        elif method == "session/new":
            state["cwd"] = params.get("cwd", ".")
            for s in params.get("mcpServers", []):
                state["mcp"] = s
            send({"jsonrpc": "2.0", "id": rid, "result": {"sessionId": "fake-1"}})
        elif method == "session/prompt":
            threading.Thread(target=handle_prompt, args=(rid, params), daemon=True).start()
        elif method == "session/cancel":
            state["cancelled"] = True


main()
