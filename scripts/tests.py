#!/usr/bin/env python3
"""
Integration tests for the OmniAgent Remove API.

Tests 7 Remove scenarios:
  1. Built-in not in plugins.yml (on disk) → should fail (cannot remove built-in)
  2. Bundled not in plugins.yml (on disk) → should succeed, remove disk
  3. Remote not in plugins.yml (on disk) → should succeed, remove disk + remote.yml
  4. Built-in in plugins.yml as source=built-in → should fail (cannot remove built-in)
  5. Bundled in plugins.yml as source=bundled → should succeed, remove disk + YAML
  6. Remote in plugins.yml as source=remote → should succeed, remove disk + YAML + remote.yml
  7. In plugins.yml but not on disk (any source) → should succeed, remove YAML only

Plus file upload tests:
  8. Upload 3 files via explorer, verify files created in data/uploads/
  9. Create kanban task, upload 2 files scoped to task, verify files created

|Usage: python3 tests.py [--restore]
  --restore: Attempt to restore the plugins.yml from git (for running after destructive tests)

Note: Tests 2, 3, 5, 6 are destructive — they REMOVE real plugins. Run against
a test environment or accept that some plugins will be missing afterward.

Git hygiene: At start, verifies omni-stack repo has no unstaged changes (raises Error if dirty).
At the end, discards any unstaged changes created during test execution, even on failure.
"""

import sys
import json
import urllib.request
import urllib.error
import os
import io
import uuid

OMNIAGENT = "http://localhost:8080"
DASHBOARD = "http://dashboard:3001"

# ── Test state ──
_UPLOAD_FILES = []       # files created by upload tests
_KANBAN_DIR = "/opt/workspace/omni-stack/data/kanban"
_UPLOADS_DIR = "/opt/workspace/omni-stack/data/uploads"

# ── Helpers ──

def api_get(base, path):
    resp = urllib.request.urlopen(f"{base}{path}")
    return json.loads(resp.read())

def api_post(base, path, body=None, files=None):
    if files:
        # multipart form upload
        boundary = uuid.uuid4().hex
        data = b""
        for field_name, filename, content in files:
            data += f"--{boundary}\r\n".encode()
            data += f'Content-Disposition: form-data; name="{field_name}"; filename="{filename}"\r\n'.encode()
            data += b"Content-Type: application/octet-stream\r\n\r\n"
            data += content + b"\r\n"
        data += f"--{boundary}--\r\n".encode()
        req = urllib.request.Request(
            f"{base}{path}",
            data=data,
            method="POST",
            headers={"Content-Type": f"multipart/form-data; boundary={boundary}"},
        )
    else:
        req = urllib.request.Request(
            f"{base}{path}",
            data=json.dumps(body).encode() if body else None,
            method="POST",
            headers={"Content-Type": "application/json"},
        )
    try:
        resp = urllib.request.urlopen(req)
        return json.loads(resp.read())
    except urllib.error.HTTPError as e:
        body = json.loads(e.read().decode())
        raise AssertionError(f"POST {path} failed (HTTP {e.code}): {body}")

def api_delete(path):
    """Return (success_bool, response_data) regardless of HTTP status"""
    req = urllib.request.Request(f"{OMNIAGENT}/api{path}", method="DELETE")
    try:
        resp = urllib.request.urlopen(req)
        body = json.loads(resp.read())
        return True, body
    except urllib.error.HTTPError as e:
        body = json.loads(e.read().decode())
        return False, body

def find_plugin(source, status=None, skip_duplicated=True):
    """Find a plugin by source. Returns name or None."""
    plugins = api_get(OMNIAGENT, "/api/plugins")["data"]
    for p in plugins:
        if p.get("source") == source:
            if status and p.get("status") != status:
                continue
            if skip_duplicated and p.get("is_duplicated"):
                continue
            return p["name"]
    return None

# ── Cleanup helpers ──

def clear_dir(dirpath):
    """Remove all files and directories under dirpath."""
    import shutil
    if os.path.exists(dirpath):
        shutil.rmtree(dirpath)
    os.makedirs(dirpath, exist_ok=True)

def check_upload_file_exists(rel_path, dirpath):
    """Check that a file exists under dirpath/rel_path."""
    full_path = os.path.join(dirpath, rel_path)
    if os.path.isfile(full_path):
        return True, f"file exists at {rel_path}"
    return False, f"file NOT found at {rel_path}"

# ── Test framework ──

tests_passed = 0
tests_failed = 0
tests_skipped = 0

def check(passed, desc, detail=""):
    global tests_passed, tests_failed, tests_skipped
    if passed is True:
        print(f"  PASS: {desc}")
        tests_passed += 1
    elif passed is None:
        print(f"  SKIP: {desc}")
        tests_skipped += 1
    else:
        print(f"  FAIL: {desc} {detail}")
        tests_failed += 1

# ── Existing Remove API tests (1-7) ──

def test_1():
    """Built-in not in plugins.yml → error"""
    name = find_plugin("built-in", skip_duplicated=True)
    if not name:
        return None
    success, resp = api_delete(f"/plugins/{name}?source=built-in")
    expected_fail = not success and "cannot remove built-in" in json.dumps(resp).lower()
    return expected_fail, f"expected error, got success={success}, resp={resp}"

def test_2():
    """Bundled not in plugins.yml → remove disk, no YAML change"""
    name = find_plugin("bundled", skip_duplicated=True)
    if not name:
        return None
    success, resp = api_delete(f"/plugins/{name}?source=bundled")
    return success, f"expected success, got success={success}, resp={resp}"

def test_3():
    """Remote not in plugins.yml → remove disk + remote.yml"""
    name = find_plugin("remote", skip_duplicated=False)
    if not name:
        return None
    success, resp = api_delete(f"/plugins/{name}?source=remote")
    return success, f"expected success, got success={success}, resp={resp}"

def test_4():
    """Built-in in plugins.yml → error"""
    name = find_plugin("built-in", skip_duplicated=True)
    if not name:
        return None
    success, resp = api_delete(f"/plugins/{name}?source=built-in")
    expected_fail = not success and "cannot remove built-in" in json.dumps(resp).lower()
    return expected_fail, f"expected error, got success={success}, resp={resp}"

def test_5():
    """Bundled in plugins.yml → remove disk + YAML"""
    name = find_plugin("bundled", skip_duplicated=True)
    if not name:
        return None
    success, resp = api_delete(f"/plugins/{name}?source=bundled")
    return success, f"expected success, got success={success}, resp={resp}"

def test_6():
    """Remote in plugins.yml → remove disk + YAML + remote.yml"""
    name = find_plugin("remote", skip_duplicated=False)
    if not name:
        return None
    success, resp = api_delete(f"/plugins/{name}?source=remote")
    return success, f"expected success, got success={success}, resp={resp}"

def test_7():
    """YAML entry, no disk → remove YAML entry"""
    plugins = api_get(OMNIAGENT, "/api/plugins")["data"]
    not_found = [p for p in plugins if p.get("status") == "not_found"]
    if not not_found:
        return None
    target = not_found[0]
    name = target["name"]
    source = target.get("source", "bundled")
    success, resp = api_delete(f"/plugins/{name}?source={source}")
    return success, f"expected success, got success={success}, resp={resp}"

# ── File upload tests (8-9) ──

def test_8():
    """Upload 3 files via explorer upload API, verify files created in data/uploads/"""
    global _UPLOAD_FILES
    clear_dir(_UPLOADS_DIR)

    # Create 3 unique test files
    test_files = [
        ("files", f"test-upload-a-{uuid.uuid4().hex[:8]}.txt", b"hello from explorer test A\n"),
        ("files", f"test-upload-b-{uuid.uuid4().hex[:8]}.txt", b"hello from explorer test B\n"),
        ("files", f"test-upload-c-{uuid.uuid4().hex[:8]}.txt", b"hello from explorer test C\n"),
    ]

    try:
        result = api_post(DASHBOARD, "/api/uploads", files=test_files)
    except AssertionError as e:
        return False, str(e)

    files_out = result.get("files", [])
    if len(files_out) != 3:
        return False, f"expected 3 files in response, got {len(files_out)}: {result}"

    _UPLOAD_FILES = [f["path"] for f in files_out]

    # Verify each file exists on disk
    all_ok = True
    details = []
    for fname in _UPLOAD_FILES:
        ok, msg = check_upload_file_exists(fname, _UPLOADS_DIR)
        if not ok:
            all_ok = False
        details.append(msg)

    if all_ok:
        return True, f"3 files uploaded and verified: {', '.join(_UPLOAD_FILES)}"
    return False, "; ".join(details)


def test_9():
    """Create kanban task, upload 2 files scoped to task, verify files created"""
    global _UPLOAD_FILES
    clear_dir(_KANBAN_DIR)

    # Step 1: Create a kanban task in backlog (through dashboard proxy, which adds /api prefix)
    try:
        task_resp = api_post(DASHBOARD, "/api/kanban/tasks", {
            "title": f"Test task {uuid.uuid4().hex[:8]}",
            "body": "Upload test for kanban-scoped files",
            "priority": 0,
            "status": "backlog",
        })
    except AssertionError as e:
        return False, f"task creation failed: {e}"

    task_id = task_resp.get("data", {}).get("id", "")
    if not task_id:
        return False, f"no id in task response: {task_resp}"

    # Step 2: Upload 2 files scoped to this task
    test_files = [
        ("files", f"kanban-file-a-{uuid.uuid4().hex[:8]}.txt", b"kanban test file A\n"),
        ("files", f"kanban-file-b-{uuid.uuid4().hex[:8]}.txt", b"kanban test file B\n"),
    ]

    try:
        upload_resp = api_post(DASHBOARD, f"/api/uploads/kanban?task_id={task_id}", files=test_files)
    except AssertionError as e:
        return False, f"kanban upload failed: {e}"

    files_out = upload_resp.get("files", [])
    if len(files_out) != 2:
        return False, f"expected 2 files, got {len(files_out)}: {upload_resp}"

    _UPLOAD_FILES = [f["path"] for f in files_out]

    # Step 3: Verify each file exists under data/kanban/{task_id}/
    all_ok = True
    details = []
    for fname in _UPLOAD_FILES:
        # fname should be "task_id/filename.ext"
        ok, msg = check_upload_file_exists(fname, _KANBAN_DIR)
        if not ok:
            all_ok = False
        details.append(msg)

    if all_ok:
        return True, f"task {task_id} created, 2 files uploaded and verified under {task_id}/"
    return False, "; ".join(details)


def restore_plugins_yml():
    """Restore plugins.yml from git to undo destructive test effects"""
    _git_checkout("plugins.yml", OMNI_STACK_DIR)


# ── Git hygiene ──

OMNI_STACK_DIR = "/opt/workspace/omni-stack"

def _git_status(repo_dir):
    """Return unstaged changes as a string, or empty string if clean."""
    import subprocess
    result = subprocess.run(
        ["git", "status", "--porcelain"],
        cwd=repo_dir,
        capture_output=True,
        text=True,
    )
    return result.stdout.strip()


def _git_checkout(path, repo_dir):
    """Discard changes to path in the given repo."""
    import subprocess
    subprocess.run(
        ["git", "checkout", "--", path],
        cwd=repo_dir,
        capture_output=True,
    )


def _git_discard_all(repo_dir):
    """Discard all unstaged changes and untracked files in the given repo."""
    import subprocess
    subprocess.run(["git", "checkout", "--", "."], cwd=repo_dir, capture_output=True)
    subprocess.run(["git", "clean", "-fd"], cwd=repo_dir, capture_output=True)


def check_git_clean():
    """Verify no unstaged changes in omni-stack repo before running tests.

    The destructive tests (2, 3, 5, 6) modify plugins.yml and remove plugin
    files — ensure we start from a clean slate so we can detect what changed.
    """
    dirty = _git_status(OMNI_STACK_DIR)
    if dirty:
        raise RuntimeError(
            f"omni-stack repo has unstaged changes — cannot run tests safely:\n{dirty}"
        )


def discard_all_changes():
    """Discard all unstaged changes in omni-stack created by test execution.

    Call in finally block so it runs even when tests fail.
    """
    _git_discard_all(OMNI_STACK_DIR)

if __name__ == "__main__":
    if "--restore" in sys.argv:
        restore_plugins_yml()

    # Verify clean git state before making any changes
    check_git_clean()

    print("\n=== Remove API Tests ===\n")

    for i, test_fn in enumerate([
        ("Built-in (no YAML)", test_1),
        ("Bundled (no YAML)", test_2),
        ("Remote (no YAML)", test_3),
        ("Built-in (in YAML)", test_4),
        ("Bundled (in YAML)", test_5),
        ("Remote (in YAML)", test_6),
        ("YAML entry (no disk)", test_7),
    ], 1):
        name, fn = test_fn
        print(f"Test {i}: {name}")
        try:
            result = fn()
            if result is None:
                check(None, "no suitable plugin found")
            else:
                passed, detail = result
                check(passed, "", detail)
        except Exception as e:
            check(False, f"exception: {e}")

    print("\n=== File Upload Tests ===\n")

    for i, test_fn in enumerate([
        ("Explorer upload 3 files", test_8),
        ("Kanban task + upload 2 files", test_9),
    ], 8):
        name, fn = test_fn
        print(f"Test {i}: {name}")
        try:
            result = fn()
            if result is None:
                check(None, "no suitable state found")
            else:
                passed, detail = result
                check(passed, "", detail)
        except Exception as e:
            check(False, f"exception: {e}")

    print(f"\n=== Results: {tests_passed} passed, {tests_failed} failed, {tests_skipped} skipped ===\n")

    # Discard any unstaged changes created by test execution — runs even on failure
    discard_all_changes()

    sys.exit(0 if tests_failed == 0 else 1)
