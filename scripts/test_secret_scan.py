#!/usr/bin/env python3
"""Self-test for scripts/secret-scan.py (code-plan C9 + external-plan X7).

Plants one sample per secret class in a throwaway git repo and asserts the
scanner reports exactly that rule and exits 1 (the pre-commit hook then blocks
the commit). It then plants the DOCUMENTED, secret-free forms (placeholder
templates, secrets-store / env interpolation, generic identifiers) and asserts
the scanner stays clean, so the guardrail does not block legitimate usage.

Run from the repo root:  python3 scripts/test_secret_scan.py
Exit code 0 = all cases pass, 1 = at least one case failed.

C9 owner task: task_omnidev_code_plan_c9_roll_out_never_commit.
External-plan X7: Projects/Omniagent/Omniagent-External-Improvement-Plan.md.
"""
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
SCANNER = os.path.join(HERE, "secret-scan.py")
if not os.path.isfile(SCANNER):
    sys.exit("secret-scan.py not found next to this test")

# Samples are built from fragments so this file itself never carries a full
# sample literal (it is also in the scanner's SKIP_FILES list).
APP_PW = " ".join(["abcd", "efgh", "ijkl", "mnop"])          # 4x4 app password
HEX32 = "".join(["0123456789abcdef", "0123456789abcdef"])    # 32 hex chars
HEX32B = "".join(["fedcba9876543210", "fedcba9876543210"])
BASE32 = "".join(["MFRGGZDF", "MZTWQ2LK"])                   # 16 base32 chars
LITERAL_PW = "".join(["Zx9", "Qm2", "Lp7T"])

# (expected rule, file name, file content) - the file name deliberately avoids
# the .env/secret-name rules so the CONTENT rule is the one being asserted.
SAMPLES = [
    ("email-app-password", "himalaya-config.toml",
     '[accounts.gmail]\nemail = "user@example.com"\npassword = "%s"\n' % APP_PW),
    ("email-password-literal", "himalaya-alt.toml",
     '[accounts.gmail]\npassword = "%s"\n' % LITERAL_PW),
    ("twilio-api-key", "sms-backend.conf",
     "TWILIO_API_KEY=SK%s\n" % HEX32),
    ("twilio-account-sid", "sms-backend.conf",
     "TWILIO_ACCOUNT=AC%s\n" % HEX32B),
    ("twilio-auth-token", "sms-backend.conf",
     'twilio_auth_token = "%s"\n' % HEX32),
    ("totp-base32-secret", "totp.conf",
     "TOTP_SECRET=%s\n" % BASE32),
    ("totp-base32-json-secret", "totp-entry.json",
     '{"secret": "%s", "issuer": "example", "account": "user@example.com"}\n'
     % BASE32),
    ("modem-sim-pin", "modem.conf",
     "SMS_PIN=4821\n"),
    ("playwright-storage-state-file", "demo.storage-state.json",
     '{"cookies": [], "origins": []}\n'),
    ("playwright-storage-state-file", "sessions.json",
     '{"cookies": [], "origins": []}\n'),
    ("secrets-file", "secrets",
     "GITHUB_PASSWORD=%s\n" % LITERAL_PW),
    # Regression control: the pre-X7 classes must keep firing.
    ("env-file", ".env", "FOO=bar\n"),
    ("github-token", "notes.txt", "token = ghp_%s\n" % HEX32),
]

# (file name, file content) - must stay clean.
CLEAN = [
    ("himalaya-config.toml.example",
     '[accounts.gmail]\nemail = "user@example.com"\n'
     'password.command = "pass show gmail"\n'),
    ("himalaya-secret-store.toml",
     '[accounts.gmail]\npassword = "$secret:GMAIL_APP_PASSWORD"\n'),
    ("totp.env.example", "TOTP_SECRET=${TOTP_SECRET}\n"),
    ("notes.md", "sim_pin_required = false\npin = 1234\n"),
    # Public RFC 6238 vector used by the robustness harness: NOT a secret.
    ("x6-robustness-fixture.py",
     'TOTP_SECRET = "%s"  # RFC 6238 public test vector\n'
     % "".join(["GEZDGNBVGY3TQOJQ", "GEZDGNBVGY3TQOJQ"])),
    ("session.spec.ts",
     "const s = await ctx.storageState({ path: '/pw/state/x.json' });\n"),
]


def _reset(repo):
    for entry in os.listdir(repo):
        if entry == ".git":
            continue
        path = os.path.join(repo, entry)
        shutil.rmtree(path) if os.path.isdir(path) else os.remove(path)


def _write(repo, name, content):
    path = os.path.join(repo, name)
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(content)


def _scan(repo):
    proc = subprocess.run([sys.executable, SCANNER, "--repo", repo],
                          capture_output=True, text=True)
    return proc.returncode, proc.stdout + proc.stderr


def main():
    failures = []
    tmp = tempfile.mkdtemp(prefix="secret-scan-selftest-")
    try:
        subprocess.run(["git", "init", "-q", tmp], check=True,
                       capture_output=True)
        for rule, name, content in SAMPLES:
            _reset(tmp)
            _write(tmp, name, content)
            rc, out = _scan(tmp)
            if rc == 1 and ("SECRET-SCAN %s:" % rule) in out:
                print("PASS  %-32s %s" % (rule, name))
            else:
                failures.append("rule %s did not fire for %s (rc=%s)"
                                % (rule, name, rc))
                print("FAIL  %-32s %s (rc=%s)" % (rule, name, rc))
        for name, content in CLEAN:
            _reset(tmp)
            _write(tmp, name, content)
            rc, out = _scan(tmp)
            if rc == 0:
                print("PASS  %-32s %s" % ("(clean)", name))
            else:
                failures.append("clean case %s tripped: %s"
                                % (name, out.strip().replace("\n", " | ")))
                print("FAIL  %-32s %s -> %s" % ("(clean)", name,
                                                out.strip().replace("\n", " | ")))
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    print("secret-scan self-test: %d case(s) checked, %d failure(s)"
          % (len(SAMPLES) + len(CLEAN), len(failures)))
    for f in failures:
        print("  - %s" % f)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
