#!/usr/bin/env python3
"""noop-full - standalone provider implementing the omniagent provider plugin protocol.

Communicates via JSON-lines over stdin/stdout. No api_mode dependency on omniagent.

Protocol:
  1. Agent → Plugin: {"id": 1, "method": "initialize", "params": {}}
     Plugin → Agent: {"id": 1, "result": {"name": "noop-full", "models": ["test-model-1", "test-model-2", "test-truncate"]}}
  2. Agent → Plugin: {"id": 2, "method": "complete", "params": {"model": "...", "messages": [...]}}
     Plugin → Agent: {"id": 2, "result": {"content": "...", "usage": {...}}}
"""

import json
import sys
import uuid


def handle_initialize(req_id):
    return {
        "id": req_id,
        "result": {
            "name": "noop-full",
            "models": ["test-model-1", "test-model-2", "test-truncate"],
        },
    }


def handle_complete(req_id, params):
    model = params.get("model", "test-model-1")
    messages = params.get("messages", [])

    if model == "test-truncate":
        # Regression fixture for the workflow-routing truncation fix (Part 2):
        # simulate a response cut off by the output budget - prose with NO tool
        # call and finish_reason="length". A truncated response must NOT be
        # treated as a completed step / final answer (live incident
        # task_18cea6054b6e6e73/thread 135: reviewer cut mid-sentence → task
        # wrongly advanced to done).
        return {
            "id": req_id,
            "result": {
                "content": (
                    "This response was truncated by the output token limit. "
                    "The agent intended to emit a routing tool call but was cut "
                    "off mid-emission (finish_reason=length)."
                ),
                "reasoning": None,
                "tool_calls": [],
                "finish_reason": "length",
                "usage": {
                    "prompt_tokens": 0,
                    "completion_tokens": 0,
                },
            },
        }

    # Extract last user message to quote
    user_msg = ""
    for msg in reversed(messages):
        if msg.get("role") == "user":
            user_msg = msg.get("content", "")
            break

    quoted_lines = [f"> {line}" for line in user_msg.split("\n")]
    quoted = "\n".join(quoted_lines)

    content = (
        f"This is a reply from the **noop-full** provider "
        f"(standalone subprocess, no api_mode dependency) "
        f"using model **{model}**.\n\n"
        f"Your message:\n\n{quoted}"
    )

    return {
        "id": req_id,
        "result": {
            "content": content,
            "reasoning": None,
            "tool_calls": [],
            "usage": {
                "prompt_tokens": 0,
                "completion_tokens": 0,
            },
        },
    }


def handle_list_models(req_id):
    return {
        "id": req_id,
        "result": {
            "models": ["test-model-1", "test-model-2", "test-truncate"],
        },
    }


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue

        try:
            req = json.loads(line)
        except json.JSONDecodeError as e:
            print(json.dumps({"error": {"code": -32700, "message": f"Parse error: {e}"}}), flush=True)
            continue

        req_id = req.get("id")
        method = req.get("method", "")
        params = req.get("params", {})

        try:
            if method == "initialize":
                response = handle_initialize(req_id)
            elif method == "complete":
                response = handle_complete(req_id, params)
            elif method == "list_models":
                response = handle_list_models(req_id)
            else:
                response = {
                    "id": req_id,
                    "error": {"code": -32601, "message": f"Method not found: {method}"},
                }
        except Exception as e:
            response = {
                "id": req_id,
                "error": {"code": -32603, "message": f"Internal error: {e}"},
            }

        print(json.dumps(response), flush=True)


if __name__ == "__main__":
    main()