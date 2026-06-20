#!/usr/bin/env python3
"""
Test Python platform plugin -- implements the OmniAgent platform plugin protocol.

Reads JSON lines from stdin, writes JSON lines to stdout.
Supports: initialize, deliver, edit_message, delete_message.
"""

import json
import sys
import logging
import os

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [test-python] %(levelname)s %(message)s",
)
log = logging.getLogger("platform")


def main():
    log.info("Test platform plugin starting (PID=%d)", os.getpid())

    message_counter = 0

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue

        try:
            request = json.loads(line)
        except json.JSONDecodeError as e:
            log.error("Failed to parse JSON: %s", e)
            continue

        method = request.get("method", "")
        req_id = request.get("id")

        if method == "initialize":
            # Respond with plugin info
            response = {
                "id": req_id,
                "result": {
                    "name": "test-python",
                    "capabilities": {
                        "inbound": False,
                        "outbound": True,
                    },
                },
            }
            sys.stdout.write(json.dumps(response) + "\n")
            sys.stdout.flush()
            log.info("Initialized: test-python")

        elif method == "deliver":
            message_counter += 1
            params = request.get("params", {})
            resource = params.get("resource_identifier", "")
            content = params.get("content", "")
            msg_type = params.get("msg_type", "")
            log.info(
                "Deliver [%d] to %s (type=%s): %s",
                message_counter,
                resource,
                msg_type,
                content[:80],
            )

            response = {
                "id": req_id,
                "result": {
                    "delivered": True,
                    "external_id": "test-" + str(message_counter),
                },
            }
            sys.stdout.write(json.dumps(response) + "\n")
            sys.stdout.flush()

        elif method == "edit_message":
            params = request.get("params", {})
            resource = params.get("resource_identifier", "")
            external_id = params.get("external_id", "")
            content = params.get("content", "")
            log.info(
                "Edit message %s in %s: %s",
                external_id,
                resource,
                content[:80],
            )

            response = {
                "id": req_id,
                "result": {
                    "edited": True,
                },
            }
            sys.stdout.write(json.dumps(response) + "\n")
            sys.stdout.flush()

        elif method == "delete_message":
            params = request.get("params", {})
            resource = params.get("resource_identifier", "")
            external_id = params.get("external_id", "")
            log.info("Delete message %s in %s", external_id, resource)

            response = {
                "id": req_id,
                "result": {
                    "deleted": True,
                },
            }
            sys.stdout.write(json.dumps(response) + "\n")
            sys.stdout.flush()

        else:
            log.warning("Unknown method: %s", method)
            if req_id is not None:
                response = {
                    "id": req_id,
                    "error": {
                        "code": -1,
                        "message": f"Unknown method: {method}",
                    },
                }
                sys.stdout.write(json.dumps(response) + "\n")
                sys.stdout.flush()

    log.info("Test platform plugin shutting down (stdin closed)")


if __name__ == "__main__":
    main()
