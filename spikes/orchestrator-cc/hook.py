import json, sys, os, time
d = os.environ["HOOK_DIR"]; budget = int(os.environ.get("INLINE_BUDGET", "3"))
ev = json.load(sys.stdin)
with open(os.path.join(d, "hooks.log"), "a") as f: f.write(json.dumps({k: ev.get(k) for k in ("hook_event_name","tool_name","session_id")}) + "\n")
counter = os.path.join(d, "count")
name = ev.get("hook_event_name")
if name == "UserPromptSubmit":
    open(counter, "w").write("0")
    ctx = "STATE SNAPSHOT (injected by hook): time=2026-11-22T19:30Z; active tasks: none; waiting on you: none; sentinel=PAPAYA-42"
    print(json.dumps({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": ctx}}))
elif name == "PreToolUse":
    n = int(open(counter).read() or 0) + 1 if os.path.exists(counter) else 1
    open(counter, "w").write(str(n))
    tool = ev.get("tool_name", "")
    if n > budget and not tool.endswith("delegate_task"):
        print(json.dumps({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny",
            "permissionDecisionReason": f"Inline budget exhausted ({budget} tool calls this turn). Stop looking things up: either answer with what you have or call delegate_task with your findings in the goal."}}))
