"""Deterministic ACP transport fixture; never contacts a model provider."""
import json
import os
import sys
import time


def send(value):
    print(json.dumps(dict(jsonrpc="2.0", **value)), flush=True)


def update(value):
    send(dict(method="session/update", params=dict(sessionId="fixture", update=value)))


recipe = {}
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        result = dict(protocolVersion=1, agentCapabilities={})
    elif method == "_goose/unstable/recipes/encode":
        recipe = request["params"]["recipe"]
        result = dict(deeplink="fixture")
    elif method == "session/new":
        result = dict(sessionId="fixture")
    elif method == "session/prompt":
        log = os.environ["FIXTURE_LOG"]
        try:
            with open(log) as stream:
                attempts = sum(1 for _ in stream)
        except FileNotFoundError:
            attempts = 0
        with open(log, "a") as stream:
            stream.write(json.dumps(dict(turns=os.environ["GOOSE_MAX_TURNS"], extensions=recipe["extensions"], prompt=request["params"]["prompt"])) + "\n")
        mode = os.environ.get("FIXTURE_MODE", "recovery")
        update(dict(sessionUpdate="agent_message_chunk", content=dict(type="text", text="Inspected source; regression did not finish.")))
        if mode == "stall":
            time.sleep(30)
        if mode == "deadline":
            time.sleep(0.6 if attempts == 0 else 30)
        if mode in ("disconnect", "failed_output"):
            if mode == "failed_output":
                output = json.loads(os.environ["FIXTURE_OUTPUT"])
                update(dict(sessionUpdate="tool_call", toolCallId="final", _meta=dict(goose=dict(toolCall=dict(toolName="final_output"))), rawInput=output, status="completed"))
            sys.exit(0)
        if mode == "empty" or (mode in ("recovery", "deadline") and attempts == 0):
            result = dict(stopReason="max_turn_requests")
        else:
            output = json.loads(os.environ["FIXTURE_OUTPUT"])
            update(dict(sessionUpdate="tool_call", toolCallId="final", _meta=dict(goose=dict(toolCall=dict(toolName="final_output"))), rawInput=output, status="pending"))
            update(dict(sessionUpdate="tool_call_update", toolCallId="final", status="completed"))
            result = dict(stopReason="end_turn")
    elif method == "session/cancel":
        sys.exit(0)
    else:
        continue
    send(dict(id=request["id"], result=result))
