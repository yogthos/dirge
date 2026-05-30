#!/usr/bin/env python3
"""
Mock DAP adapter — speaks the Content-Length framed Debug Adapter Protocol
over stdio. Used by dirge integration tests.

Implements the minimum set of DAP requests needed by the integration smoke
test: initialize, launch, setBreakpoints, configurationDone, threads,
stackTrace, scopes, variables, evaluate, continue, terminate, disconnect.

Logs to stderr so stdout remains a clean DAP stream.
"""

import json
import sys
import os


def log(msg):
    print(f"[mock-dap] {msg}", file=sys.stderr, flush=True)


def read_frame():
    """Read a single Content-Length framed message from stdin."""
    header = b""
    while not header.endswith(b"\r\n\r\n"):
        byte = sys.stdin.buffer.read(1)
        if not byte:
            log("stdin closed")
            sys.exit(0)
        header += byte
    header_str = header.decode("utf-8")
    content_length = 0
    for line in header_str.split("\r\n"):
        if line.lower().startswith("content-length:"):
            content_length = int(line.split(":", 1)[1].strip())
    if content_length == 0:
        log(f"zero-length frame, header was: {header_str!r}")
        return None
    body = sys.stdin.buffer.read(content_length)
    return json.loads(body)


def write_frame(obj):
    """Write a single Content-Length framed message to stdout."""
    body = json.dumps(obj, separators=(",", ":")).encode("utf-8")
    header = f"Content-Length: {len(body)}\r\n\r\n".encode("utf-8")
    sys.stdout.buffer.write(header + body)
    sys.stdout.buffer.flush()


def send_response(request_seq, command, body=None, success=True, message=None):
    resp = {
        "type": "response",
        "seq": next_seq(),
        "request_seq": request_seq,
        "command": command,
        "success": success,
    }
    if body is not None:
        resp["body"] = body
    if message is not None:
        resp["message"] = message
    write_frame(resp)


def send_event(event_type, body=None):
    evt = {
        "type": "event",
        "seq": next_seq(),
        "event": event_type,
    }
    if body is not None:
        evt["body"] = body
    write_frame(evt)


_seq = 0


def next_seq():
    global _seq
    _seq += 1
    return _seq


# ---- Fake debuggee state ----
stopped_thread_id = 1
frame_id_counter = 1000
variable_ref_counter = 2000
breakpoints_set = False


def main():
    global breakpoints_set, frame_id_counter, variable_ref_counter

    log("mock DAP adapter starting")

    # 1. initialize
    msg = read_frame()
    assert msg["command"] == "initialize", f"expected initialize, got {msg}"
    send_response(
        msg["seq"],
        "initialize",
        body={
            "supportsConfigurationDoneRequest": True,
            "supportsEvaluateForHovers": True,
            "supportsStepInTargetsRequest": True,
            "supportsConditionalBreakpoints": True,
            "supportsFunctionBreakpoints": True,
            "supportsSetVariable": True,
            "supportsTerminateRequest": True,
            "supportsDisassembleRequest": False,
            "supportTerminateDebuggee": True,
        },
    )
    send_event("initialized")

    # 2. launch
    msg = read_frame()
    assert msg["command"] == "launch", f"expected launch, got {msg}"
    send_response(msg["seq"], "launch")
    send_event(
        "stopped",
        body={"reason": "entry", "threadId": stopped_thread_id, "allThreadsStopped": True},
    )

    # 3. setBreakpoints
    msg = read_frame()
    assert msg["command"] == "setBreakpoints", f"expected setBreakpoints, got {msg}"
    bps = msg.get("arguments", {}).get("breakpoints", [])
    breakpoints_set = len(bps) > 0
    send_response(
        msg["seq"],
        "setBreakpoints",
        body={
            "breakpoints": [
                {"id": i + 1, "verified": True, "line": bp.get("line", 0)}
                for i, bp in enumerate(bps)
            ]
        },
    )

    # 4. configurationDone
    msg = read_frame()
    assert msg["command"] == "configurationDone", f"expected configurationDone, got {msg}"
    send_response(msg["seq"], "configurationDone")

    # ---- Query phase: threads → stackTrace → scopes → variables → evaluate ----

    # 5. threads
    msg = read_frame()
    assert msg["command"] == "threads", f"expected threads, got {msg}"
    send_response(
        msg["seq"],
        "threads",
        body={"threads": [{"id": stopped_thread_id, "name": "MainThread"}]},
    )

    # 6. stackTrace
    msg = read_frame()
    assert msg["command"] == "stackTrace", f"expected stackTrace, got {msg}"
    frame_id_1 = frame_id_counter
    frame_id_counter += 1
    send_response(
        msg["seq"],
        "stackTrace",
        body={
            "stackFrames": [
                {
                    "id": frame_id_1,
                    "name": "main",
                    "source": {"name": "test.py", "path": "/tmp/test.py"},
                    "line": 10,
                    "column": 0,
                }
            ],
            "totalFrames": 1,
        },
    )

    # 7. scopes
    msg = read_frame()
    assert msg["command"] == "scopes", f"expected scopes, got {msg}"
    var_ref = variable_ref_counter
    variable_ref_counter += 1
    send_response(
        msg["seq"],
        "scopes",
        body={
            "scopes": [
                {
                    "name": "Locals",
                    "variablesReference": var_ref,
                    "expensive": False,
                }
            ]
        },
    )

    # 8. variables
    msg = read_frame()
    assert msg["command"] == "variables", f"expected variables, got {msg}"
    send_response(
        msg["seq"],
        "variables",
        body={
            "variables": [
                {"name": "x", "value": "42", "type": "int", "variablesReference": 0}
            ]
        },
    )

    # 9. evaluate
    msg = read_frame()
    assert msg["command"] == "evaluate", f"expected evaluate, got {msg}"
    send_response(
        msg["seq"],
        "evaluate",
        body={"result": "2", "type": "int", "variablesReference": 0},
    )

    # 10. continue
    msg = read_frame()
    assert msg["command"] == "continue", f"expected continue, got {msg}"
    send_response(
        msg["seq"],
        "continue",
        body={"allThreadsContinued": True},
    )
    send_event(
        "stopped",
        body={"reason": "breakpoint", "threadId": stopped_thread_id, "allThreadsStopped": True},
    )

    # 11. terminate
    msg = read_frame()
    assert msg["command"] == "terminate", f"expected terminate, got {msg}"
    send_response(msg["seq"], "terminate")
    send_event("terminated")

    # 12. disconnect
    msg = read_frame()
    assert msg["command"] == "disconnect", f"expected disconnect, got {msg}"
    send_response(msg["seq"], "disconnect")
    send_event("exited", body={"exitCode": 0})

    log("mock DAP adapter exiting normally")


if __name__ == "__main__":
    try:
        main()
    except Exception as e:
        log(f"FATAL: {e}")
        sys.exit(1)
