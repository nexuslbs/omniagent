#!/usr/bin/env python3
"""fetch MCP server — HTTP GET tool via Python requests (stdlib)."""

import sys
import os
import urllib.request
import urllib.error

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "sdk"))
from server import McpServer

server = McpServer(name="fetch", version="0.1.0")


@server.tool(
    name="fetch",
    description="FETCH/HTTP GET a URL from the internet. Use this to download web pages, API responses, or any HTTP-accessible content. Does NOT work with file:// URLs or local files.",
    input_schema={
        "type": "object",
        "properties": {
            "url": {
                "type": "string",
                "description": "The URL to fetch",
            },
        },
        "required": ["url"],
    },
)
def handle_fetch(arguments):
    url = arguments.get("url", "")
    if not url:
        return ("Error: Missing 'url' argument", True)

    req = urllib.request.Request(
        url,
        headers={"User-Agent": "OmniAgent/1.0"},
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            body = resp.read().decode("utf-8", errors="replace")
            status = resp.status
            # Truncate to ~50K chars
            MAX_CHARS = 50_000
            if len(body) > MAX_CHARS:
                body = body[:MAX_CHARS] + f"\n\n[... truncated from {len(body)} to ~{MAX_CHARS} chars]"
            text = f"HTTP {status}\n\n{body}"
            return (text, status >= 400)
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace") if e.fp else str(e)
        return (f"HTTP {e.code} {e.reason}\n\n{body}", True)
    except urllib.error.URLError as e:
        return (f"Error fetching URL: {e.reason}", True)
    except Exception as e:
        return (f"Error: {e}", True)


if __name__ == "__main__":
    server.run()
