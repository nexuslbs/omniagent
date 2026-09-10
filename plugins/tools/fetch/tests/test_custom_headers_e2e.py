#!/usr/bin/env python3
"""End-to-end tests for the fetch plugin's `allow_custom_headers` config gate.

Drives the real `mcp-server-fetch` binary over stdio JSON-RPC (the same protocol
the omniagent core uses) against a hermetic local echo / basic-auth HTTP server,
so every assertion is about bytes that actually crossed the socket - not about
source code.

Covered (task task_omnidev_fetch_plugin_add_config_allow_custom, gates 1-5):

  gate 1  config false  -> a non-empty `headers` argument is rejected with an
                           informative error naming `allow_custom_headers`;
                           an empty object stays a documented no-op;
                           `allow_unsafe_methods` semantics are unchanged.
  gate 2  config true   -> custom headers are really sent (echoed by the server).
  gate 3  Basic auth    -> the /basic endpoint answers 401 without the header and
                           200 with `Authorization: Basic $secret:<name>`, i.e. a
                           credential never has to appear in the tool call.
  gate 4  expansion     -> `$env:VAR` and `$secret:NAME` are expanded at call time;
                           values expanded from `$secret:` are redacted from the
                           tool result (the raw secret never appears).
  gate 5  safety        -> CR/LF in a header name/value, a >8 KiB value, and >32
                           headers are rejected with informative errors; an
                           unknown secret / unset env var / missing `database_url`
                           produce informative errors instead of silent failure.

All test values are DERIVED (sha256 of a fixed passphrase), never real
credentials; only sha256 prefixes are printed, so digests can be compared across
processes without disclosing values.

Usage (inside the dev container, or on a host with a built plugin binary):

    python3 plugins/tools/fetch/tests/test_custom_headers_e2e.py \
        [--bin target/release/mcp-server-fetch] [--secret-name FETCH_E2E_BASIC]

The `$secret:` cases need a `database_url` (env DATABASE_URL by default) pointing
at an omniagent DB that holds a secret named by --secret-name; the script seeds
that secret through the agent HTTP API (http://localhost:8080) and SKIPS the two
secret cases if the API/DB is unavailable.

Exit code: 0 = all executed cases pass, 1 = at least one failure.
"""

import argparse
import base64
import hashlib
import http.server
import json
import os
import subprocess
import sys
import threading
import urllib.error
import urllib.request

HOST = "127.0.0.1"
PORT = 18080

USER = "e2euser"
PASSWORD = "e2e-" + hashlib.sha256(b"fetch-e2e-passphrase").hexdigest()[:16]
BASIC = base64.b64encode(f"{USER}:{PASSWORD}".encode()).decode()
AUTH = "Basic " + BASIC
BASIC_SHA = hashlib.sha256(BASIC.encode()).hexdigest()[:16]

ENV_VAR = "FETCH_E2E_ENV"
ENV_VALUE = "env-visible-value"
MISSING_SECRET = "FETCH_E2E_MISSING"

API = "http://localhost:8080"
MIN_PATH = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
PLUGIN_DIR = os.path.abspath(os.path.join(SCRIPT_DIR, ".."))
REPO_ROOT = os.path.abspath(os.path.join(PLUGIN_DIR, "..", "..", ".."))

PASS, FAIL, SKIP = "PASS", "FAIL", "SKIP"


def sha16(text):
    return hashlib.sha256(text.encode()).hexdigest()[:16]


# ---------------------------------------------------------------------------
# hermetic target server
# ---------------------------------------------------------------------------
class Target(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _send(self, code, payload):
        data = json.dumps(payload).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        auth = self.headers.get("Authorization")
        if self.path.startswith("/basic"):
            if auth == AUTH:
                self._send(200, {"result": "AUTHORIZED", "auth_sha256[:16]": sha16(auth)})
            else:
                self._send(401, {"result": "DENIED"})
            return
        self._send(200, {"path": self.path, "headers": {k: v for k, v in self.headers.items()}})

    def log_message(self, *args):
        pass


def start_target_server(port=PORT):
    server = http.server.ThreadingHTTPServer((HOST, port), Target)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


# ---------------------------------------------------------------------------
# minimal MCP stdio client
# ---------------------------------------------------------------------------
class Mcp:
    def __init__(self, binary, cwd, env):
        self.proc = subprocess.Popen(
            [binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, env=env, cwd=cwd, text=True, bufsize=1,
        )
        self.n = 0

    def call(self, method, params=None):
        self.n += 1
        msg = {"jsonrpc": "2.0", "id": self.n, "method": method}
        if params is not None:
            msg["params"] = params
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError("plugin closed stdout")
            try:
                resp = json.loads(line)
            except ValueError:
                continue
            if resp.get("id") == self.n:
                return resp

    def start(self):
        init = self.call(
            "initialize",
            {"protocolVersion": "2024-11-05", "capabilities": {},
             "clientInfo": {"name": "fetch-custom-headers-e2e", "version": "1.0"}},
        )
        self.proc.stdin.write(
            json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
        self.proc.stdin.flush()
        return init

    def tool_call(self, args, tool="fetch"):
        return self.call("tools/call", {"name": tool, "arguments": args})

    def stop(self):
        try:
            self.proc.stdin.close()
            self.proc.kill()
        except Exception:
            pass


def plugin_env(allow_headers, binary_env=None, unsafe="true"):
    """Environment passed to the plugin = the config_schema keys (see plugin.json)."""
    env = {
        "PATH": MIN_PATH,
        "HOME": PLUGIN_DIR,
        "allow_unsafe_methods": unsafe,
        "allow_custom_headers": "true" if allow_headers else "false",
    }
    if binary_env:
        env.update(binary_env)
    return env


def text_of(resp):
    result = resp.get("result") or {}
    content = result.get("content")
    if isinstance(content, list) and content:
        return content[0].get("text", "")
    return json.dumps(result)


def is_err(resp):
    return bool((resp.get("result") or {}).get("isError"))


# ---------------------------------------------------------------------------
# secret seeding (dev/test DB only)
# ---------------------------------------------------------------------------
def seed_secret(name):
    body = json.dumps({"name": name, "fieldType": "password", "value": BASIC}).encode()
    req = urllib.request.Request(API + "/secrets", data=body, method="POST",
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            return resp.status in (200, 201)
    except urllib.error.HTTPError as err:
        if err.code not in (400, 409, 500):
            return False
        req = urllib.request.Request(
            API + "/secrets/" + name, data=json.dumps({"value": BASIC}).encode(),
            method="PUT", headers={"Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=10) as resp:
                return resp.status == 200
        except Exception:
            return False
    except Exception:
        return False


# ---------------------------------------------------------------------------
# cases
# ---------------------------------------------------------------------------
class Runner:
    def __init__(self, binary, secret_name, database_url):
        self.binary = binary
        self.secret_name = secret_name
        self.database_url = database_url
        self.passed = 0
        self.failed = 0
        self.skipped = 0

    def case(self, name, env, args, check, tool="fetch"):
        m = Mcp(self.binary, PLUGIN_DIR, env)
        try:
            m.start()
            resp = m.tool_call(args, tool=tool)
        except Exception as exc:  # harness/plugin crash
            self.record(name, FAIL, f"exception: {exc!r}")
            return
        finally:
            m.stop()
        text, err = text_of(resp), is_err(resp)
        try:
            ok, detail = check(text, err)
        except Exception as exc:
            self.record(name, FAIL, f"check raised {exc!r}; text={text!r}")
            return
        self.record(name, PASS if ok else FAIL, detail)

    def record(self, name, status, detail):
        detail = " ".join(str(detail).split())
        print(f"[{status}] {name}: {detail[:400]}", flush=True)
        if status == PASS:
            self.passed += 1
        elif status == FAIL:
            self.failed += 1
        else:
            self.skipped += 1

    # -- gates ------------------------------------------------------------
    def run(self):
        url = f"http://{HOST}:{PORT}"

        # gate 1: disabled config rejects the headers argument informatively
        def c_disabled(text, err):
            return (err and "allow_custom_headers" in text and "disabled" in text), \
                f"isError={err} text={text!r}"
        self.case("gate1_headers_disabled_rejected", plugin_env(False),
                  {"url": url + "/echo", "headers": {"X-Literal": "hello"}}, c_disabled)

        # gate 1: an empty object is a documented no-op in every mode
        def c_empty(text, err):
            return (not err and "HTTP 200" in text), f"isError={err} text={text!r}"
        self.case("gate1_empty_headers_object_allowed_when_disabled", plugin_env(False),
                  {"url": url + "/echo", "headers": {}}, c_empty)

        # gate 1: allow_unsafe_methods semantics unchanged
        def c_unsafe(text, err):
            return (err and "allow_unsafe_methods" in text), f"isError={err} text={text!r}"
        self.case("gate1_unsafe_methods_unchanged", plugin_env(True, unsafe="false"),
                  {"url": url + "/echo", "method": "POST"}, c_unsafe)

        # gate 2: literal headers really reach the server
        def c_sent(text, err):
            low = text.lower()
            ok = (not err and '"x-literal": "hello"' in low
                  and '"accept": "application/json"' in low)
            return ok, f"isError={err} text={text!r}"
        self.case("gate2_literal_headers_sent", plugin_env(True),
                  {"url": url + "/echo",
                   "headers": {"X-Literal": "hello", "Accept": "application/json"}}, c_sent)

        # gate 4a: $env: expansion (embedded)
        env = plugin_env(True, {ENV_VAR: ENV_VALUE})

        def c_env(text, err):
            return (not err and f'"x-e2e-env": "v-{ENV_VALUE}"' in text.lower()), \
                f"isError={err} text={text!r}"
        self.case("gate4_env_ref_expanded", env,
                  {"url": url + "/echo", "headers": {"X-E2e-Env": f"v-$env:{ENV_VAR}"}}, c_env)

        # gate 5: CRLF injection, oversize value, too many headers
        def c_crlf(text, err):
            return (err and "CR/LF" in text), f"isError={err} text={text!r}"
        self.case("gate5_crlf_rejected", plugin_env(True),
                  {"url": url + "/echo", "headers": {"X-Bad": "v\r\nX-Injected: evil"}}, c_crlf)

        def c_crlf_name(text, err):
            return (err and ("CR/LF" in text or "invalid header name" in text)), \
                f"isError={err} text={text!r}"
        self.case("gate5_crlf_in_name_rejected", plugin_env(True),
                  {"url": url + "/echo", "headers": {"X-Bad\r\nX-Injected": "v"}}, c_crlf_name)

        def c_big(text, err):
            return (err and "too large" in text), f"isError={err} text={text[:200]!r}"
        self.case("gate5_oversize_value_rejected", plugin_env(True),
                  {"url": url + "/echo", "headers": {"X-Big": "a" * 9000}}, c_big)

        def c_many(text, err):
            return (err and "too many" in text), f"isError={err} text={text[:200]!r}"
        self.case("gate5_too_many_headers_rejected", plugin_env(True),
                  {"url": url + "/echo",
                   "headers": {f"X-H{i}": "v" for i in range(33)}}, c_many)

        # gate 5: informative failures that must not leak or crash
        def c_missing_secret(text, err):
            return (err and MISSING_SECRET in text and "not found" in text), \
                f"isError={err} text={text!r}"
        self.case("gate5_unknown_secret_informative",
                  plugin_env(True, {"database_url": self.database_url}),
                  {"url": url + "/echo", "headers": {"X-E2e": f"$secret:{MISSING_SECRET}"}},
                  c_missing_secret)

        def c_unset_env(text, err):
            return (err and "FETCH_E2E_NOT_SET" in text and "not visible" in text), \
                f"isError={err} text={text!r}"
        self.case("gate5_unset_env_ref_informative", plugin_env(True),
                  {"url": url + "/echo", "headers": {"X-E2e": "$env:FETCH_E2E_NOT_SET"}},
                  c_unset_env)

        def c_no_db(text, err):
            return (err and "database_url" in text), f"isError={err} text={text!r}"
        self.case("gate5_secret_ref_without_database_url", plugin_env(True),
                  {"url": url + "/echo",
                   "headers": {"X-E2e": f"$secret:{self.secret_name}"}}, c_no_db)

        # gate 3/4b: $secret: expansion, redaction and real Basic auth
        have_secret = bool(self.database_url) and seed_secret(self.secret_name)
        if not have_secret:
            self.record("gate4_secret_ref_expanded_and_redacted", SKIP,
                        "no database_url / agent API; secret cases skipped")
            self.record("gate3_basic_auth_200_via_secret_ref", SKIP,
                        "no database_url / agent API; secret cases skipped")
        else:
            env_secret = plugin_env(True, {"database_url": self.database_url})

            def c_redacted(text, err):
                marker = f"***REDACTED($secret:{self.secret_name})***"
                ok = (not err and marker in text and BASIC not in text)
                return ok, (f"isError={err} raw_secret_in_result={BASIC in text} "
                            f"expected_sha256[:16]={BASIC_SHA} text={text!r}")
            self.case("gate4_secret_ref_expanded_and_redacted", env_secret,
                      {"url": url + "/echo",
                       "headers": {"X-E2e-Secret": f"$secret:{self.secret_name}"}}, c_redacted)

            def c_basic(text, err):
                ok = (not err and "HTTP 200" in text and "AUTHORIZED" in text
                      and BASIC not in text)
                return ok, (f"isError={err} raw_secret_in_result={BASIC in text} "
                            f"expected_sha256[:16]={BASIC_SHA} text={text!r}")
            self.case("gate3_basic_auth_200_via_secret_ref", env_secret,
                      {"url": url + "/basic",
                       "headers": {"Authorization": f"Basic $secret:{self.secret_name}"}}, c_basic)

            def c_401(text, err):
                return ("HTTP 401" in text and "DENIED" in text), f"isError={err} text={text!r}"
            self.case("gate3_basic_auth_401_without_header", env_secret,
                      {"url": url + "/basic"}, c_401)

        # tool schema advertises the new parameter
        m = Mcp(self.binary, PLUGIN_DIR, plugin_env(True))
        try:
            m.start()
            listing = m.call("tools/list")
            tools = (listing.get("result") or {}).get("tools") or []
            fetch_tool = next((t for t in tools if t.get("name") == "fetch"), {})
            props = sorted(((fetch_tool.get("inputSchema") or {}).get("properties") or {}))
            self.record("gate2_tools_list_exposes_headers_param",
                        PASS if "headers" in props else FAIL, f"params={props}")
        except Exception as exc:
            self.record("gate2_tools_list_exposes_headers_param", FAIL, f"exception: {exc!r}")
        finally:
            m.stop()

    def summary(self):
        total = self.passed + self.failed + self.skipped
        print(f"SUMMARY: {self.passed}/{total - self.skipped} executed cases passed"
              f"{f', {self.skipped} skipped' if self.skipped else ''}", flush=True)
        return 0 if self.failed == 0 else 1


def resolve_binary(explicit):
    """Find the built plugin binary: --bin, FETCH_PLUGIN_BIN, CARGO_TARGET_DIR, repo target/."""
    candidates = []
    if explicit:
        candidates.append(explicit)
    if os.environ.get("FETCH_PLUGIN_BIN"):
        candidates.append(os.environ["FETCH_PLUGIN_BIN"])
    target_dir = os.environ.get("CARGO_TARGET_DIR") or os.path.join(REPO_ROOT, "target")
    for profile in ("release", "debug"):
        candidates.append(os.path.join(target_dir, profile, "mcp-server-fetch"))
    for candidate in candidates:
        if os.path.isfile(candidate):
            return os.path.abspath(candidate)
    return None


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--bin", default=None)
    ap.add_argument("--secret-name", default="FETCH_E2E_BASIC")
    ap.add_argument("--database-url", default=os.environ.get("DATABASE_URL", ""))
    ap.add_argument("--port", type=int, default=PORT)
    args = ap.parse_args()

    binary = resolve_binary(args.bin)
    if binary is None:
        print("FATAL: mcp-server-fetch binary not found; pass --bin PATH "
              "(searched FETCH_PLUGIN_BIN, CARGO_TARGET_DIR, <repo>/target)",
              file=sys.stderr)
        return 2
    print(f"plugin binary: {binary}")
    print(f"plugin dir:    {PLUGIN_DIR}")
    print(f"database_url:  {'set' if args.database_url else 'unset'}")

    server = start_target_server(args.port)
    try:
        runner = Runner(binary, args.secret_name, args.database_url)
        runner.run()
        return runner.summary()
    finally:
        server.shutdown()


if __name__ == "__main__":
    sys.exit(main())
