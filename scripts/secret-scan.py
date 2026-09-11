#!/usr/bin/env python3
"""Never-commit secret scanner (code-plan C9 + external-plan X7 guardrail).

Detects secret-bearing file NAMES and high-signal secret CONTENT across
the repository. Modes:
  default   : tracked files + untracked non-ignored files (git ls-files)
  --staged  : files staged for the next commit (pre-commit hook)

Exit code 0 = clean, 1 = findings. Findings print path + rule only;
matched values are redacted and never echoed (plan 9.1: no secret
printing in scripts/logs).

C9 owner task: task_omnidev_code_plan_c9_roll_out_never_commit (code-plan
section 9.1). External-plan X7 extends that guardrail with the credential
classes the external-interaction plan introduces (plan page
Projects/Omniagent/Omniagent-External-Improvement-Plan.md, candidate X7;
code-plan section 9.5):
  - email app passwords (himalaya password / password.command values, X1),
  - SMS modem / SIM PINs and Twilio API keys / auth tokens (X2),
  - TOTP / HOTP base32 shared secrets (X3),
  - Playwright per-site storage-state files and --secrets values (X4/X5).
The same rules are mirrored in the gitleaks config (`.gitleaks.toml`) in the
repos that run the gitleaks CI job (omniagent, omni-deployer); where a repo
has no CI, this pre-commit scanner is the enforcement point.
Proof that the rules fire: `python3 scripts/test_secret_scan.py`.

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
# This scanner, its self-test and the hook installer legitimately contain
# rule text / planted secret FORMATS (never live values).
SKIP_FILES = {"secret-scan.py", "test_secret_scan.py",
              "install-pre-commit-secret-scan.sh"}
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

# Documented PUBLIC test vectors / example credentials that are NOT secrets.
# RFC 6238 Appendix B TOTP vector: used by the external-tool robustness
# harness (omni-deployer scripts/x6_robustness.py) and printed in public
# RFCs. A content match whose text CONTAINS one of these values is skipped;
# every other value still trips its rule.
ALLOW_VALUES = (
    "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ",
)

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
    # --- external-plan X7: credential classes of the external tools ---
    # Email app password (X1): a grouped app password (4x4, e.g. "abcd efgh
    # ijkl mnop") assigned to `password` or `password.command` in a
    # himalaya-style config. The group separator must repeat.
    ("email-app-password",
     re.compile(r"(?i)^[ \t]*password(?:\.command)?[ \t]*=[ \t]*[\"'][^\"'\n]*"
                r"\b[a-z]{4}([ -])[a-z]{4}\1[a-z]{4}\1[a-z]{4}\b[^\"'\n]*[\"']",
                re.M)),
    # Any other literal password value (>=8 chars) in a himalaya-style
    # config. `password.command = "pass show x"` and secrets-store / env
    # references ($secret:NAME, ${VAR}) do not match: the key must be exactly
    # `password` and the value must not start with `$`.
    ("email-password-literal",
     re.compile(r"(?i)^[ \t]*password[ \t]*=[ \t]*[\"'][^\"'\n$]{8,}[\"']",
                re.M)),
    # SMS backend (X2): Twilio API key SID (SK + 32 hex), account SID
    # (AC + 32 hex) and a 32-hex auth token next to an auth-token key.
    ("twilio-api-key",
     re.compile(r"\bSK[0-9a-fA-F]{32}\b")),
    ("twilio-account-sid",
     re.compile(r"\bAC[0-9a-fA-F]{32}\b")),
    ("twilio-auth-token",
     re.compile(r"(?i)twilio[_-]?auth[_-]?token[ \t\"'=:]+[\"']?"
                r"[0-9a-f]{32}")),
    # SMS modem / SIM PIN (X2). Keyed on the sim/modem/sms prefix so an
    # unrelated "pin" identifier does not trip.
    ("modem-sim-pin",
     re.compile(r"(?i)\b(sim|modem|sms)[_-]?pin\b[ \t\"'=:]+[\"']?\d{4,8}\b")),
    # TOTP / HOTP base32 shared secret (X3): keyed on the secret name, or the
    # secrets-store JSON layout {"secret":"<base32>",...}.
    ("totp-base32-secret",
     re.compile(r"(?i)\b(totp|otp)[_-]?secret\b[ \t\"'=:]+[\"']?"
                r"[A-Z2-7]{16,}")),
    ("totp-base32-json-secret",
     re.compile(r"[\"']secret[\"'][ \t]*:[ \t]*[\"'][A-Z2-7]{16,}[\"']")),
]

# File-name classes for the X7 web-session credentials: a Playwright
# per-site storage-state file holds live cookies + localStorage and the
# --secrets file holds the typed credential values; neither may be committed.
STORAGE_STATE_MARKERS = ("storage-state", "storage_state", "storagestate",
                         "playwright-state", "sessions.json")
SECRETS_FILE_NAMES = ("secrets", "secrets.txt", "secrets.json", ".secrets")


def name_rule(rel):
    base = os.path.basename(rel).lower()
    if base.startswith("id_rsa"):
        return "ssh-private-key"
    if base.endswith(KEY_EXT):
        return "key-material-file"
    if base == "secrets.env" or base.startswith("secrets.env."):
        return "secrets-env-file"
    if base in SECRETS_FILE_NAMES:
        return "secrets-file"
    # Playwright per-site session state (cookies/localStorage) - X7/X5.
    if (any(m in base for m in STORAGE_STATE_MARKERS)
            or base.endswith(".auth.json")):
        return "playwright-storage-state-file"
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
            m = rx.search(text)
            if m is None:
                continue
            if any(v in m.group(0) for v in ALLOW_VALUES):
                # Documented PUBLIC test vector, never a live credential.
                continue
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
