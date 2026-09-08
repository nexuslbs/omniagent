#!/usr/bin/env python3
"""Never-commit secret scanner (code-plan C9 guardrail).

Detects secret-bearing file NAMES and high-signal secret CONTENT across
the repository. Modes:
  default   : tracked files + untracked non-ignored files (git ls-files)
  --staged  : files staged for the next commit (pre-commit hook)

Exit code 0 = clean, 1 = findings. Findings print path + rule only;
matched values are redacted and never echoed (plan 9.1: no secret
printing in scripts/logs).

Usage: python3 scripts/secret-scan.py [--staged] [--repo PATH]
"""
import argparse
import os
import re
import subprocess
import sys

KEY_EXT = (".pem", ".key", ".p12", ".pfx", ".jks", ".jceks",
           ".keystore", ".credential", ".token", ".secret")
ENV_ALLOWED = (".example", ".sample", ".dist")
SKIP_DIRS = {".git", ".git-cache", "node_modules", "target", "dist",
             "__pycache__", ".venv", "coverage", ".husky/_"}
# This scanner and the hook installer legitimately contain rule text.
SKIP_FILES = {"secret-scan.py", "install-pre-commit-secret-scan.sh"}
# Committed code/test/workflow/doc fixtures that reference secret formats
# (header constants, variable interpolation like ${{ secrets.X }}, fake
# test keys) but contain NO live secret values. Content rules are skipped
# for these exact paths so the baseline stays green; gitleaks CI mirrors
# this in .gitleaks.toml. A NEW secret anywhere else is still blocked.
ALLOW_CONTENT = frozenset({
    "plugins/tools/git/src/git_sync.rs",
    "plugins/tools/git/src/main.rs",
    "plugins/tools/ssh/src/main.rs",
    "tests/api_tests.rs",
    ".github/workflows/plugins.yml",
    ".github/workflows/publish.yml",
    "scripts/tests.py",
    "profiles/omni/wiki/Memory/Promoted/git-push-auth-fallback-via-github-app-jwt.md",
    "profiles/omni/wiki/Memory/Promoted/git-push-workaround-broken-app-key.md",
    "profiles/omni/wiki/Reference/Omniagent/Git-Plugin-GitHub-App-Key.md",
    "profiles/omni/wiki/Reference/Omniagent/Redaction.md",
    "profiles/omni/wiki/log.md",
})

CONTENT_RULES = [
    ("private-key-header",
     re.compile(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY( BLOCK)?-----")),
    ("github-token",
     re.compile(r"(ghp_|ghs_|github_pat_)[A-Za-z0-9_]{20,}")),
    ("openai-style-key",
     re.compile(r"sk-[A-Za-z0-9]{20,}")),
    ("slack-token",
     re.compile(r"xox[baprs]-[A-Za-z0-9-]{10,}")),
    ("aws-access-key-id",
     re.compile(r"AKIA[0-9A-Z]{16}")),
    ("google-api-key",
     re.compile(r"AIza[0-9A-Za-z_-]{35}")),
    # Token header followed by a literal token value (>=8 token chars).
    # Does not match variable interpolation (${{ ... }}, ${VAR}) or
    # format placeholders like "x-access-token:{}".
    ("http-basic-token",
     re.compile(r"x-access-token[:=][ \t]*[A-Za-z0-9_\-.]{8,}")),
]


def name_rule(rel):
    base = os.path.basename(rel).lower()
    if base.startswith("id_rsa"):
        return "ssh-private-key"
    if base.endswith(KEY_EXT):
        return "key-material-file"
    if base == "secrets.env" or base.startswith("secrets.env."):
        return "secrets-env-file"
    if base.startswith(".env") or base.endswith(".env"):
        if base in (".env.example", ".env.sample", ".env.dist") or base.endswith(ENV_ALLOWED):
            return None
        return "env-file"
    return None


def repo_file_list(repo, staged):
    base = ["git", "-C", repo]
    if staged:
        out = subprocess.check_output(
            base + ["diff", "--cached", "--name-only", "--diff-filter=ACM", "-z"])
        return [p for p in out.decode("utf-8", "replace").split("\0") if p]
    out = subprocess.check_output(base + ["ls-files", "-z"])
    out += subprocess.check_output(
        base + ["ls-files", "--others", "--exclude-standard", "-z"])
    return [p for p in out.decode("utf-8", "replace").split("\0") if p]


def scan():
    ap = argparse.ArgumentParser()
    ap.add_argument("--staged", action="store_true")
    ap.add_argument("--repo", default=".")
    args = ap.parse_args()
    repo = os.path.abspath(args.repo)
    files = repo_file_list(repo, args.staged)
    findings = set()
    for rel in files:
        parts = rel.split("/")
        if any(p in SKIP_DIRS for p in parts[:-1]):
            continue
        if parts[-1] in SKIP_FILES:
            continue
        rule = name_rule(rel)
        if rule:
            findings.add((rel, rule))
            continue
        if rel in ALLOW_CONTENT:
            continue
        path = os.path.join(repo, rel)
        if not os.path.isfile(path):
            continue
        try:
            with open(path, "rb") as fh:
                data = fh.read()
        except OSError:
            continue
        text = data.decode("utf-8", "replace")
        for rname, rx in CONTENT_RULES:
            if rx.search(text):
                findings.add((rel, rname))
                break
    for rel, rule in sorted(findings):
        print("SECRET-SCAN %s: %s (value redacted)" % (rule, rel))
    if findings:
        print("secret-scan: %d finding(s); commit blocked. Never commit secrets."
              % len(findings))
        return 1
    print("secret-scan: clean (%d file(s) scanned)" % len(files))
    return 0


if __name__ == "__main__":
    sys.exit(scan())
