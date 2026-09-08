#!/bin/sh
# Install the never-commit secret-scan pre-commit hook (code-plan C9).
# Idempotent: safe to re-run. On every `git commit` the hook runs
# scripts/secret-scan.py --staged and blocks the commit on findings.
set -eu
root=$(git rev-parse --show-toplevel)
hook="$root/.git/hooks/pre-commit"
cat > "$hook" <<'HOOK_EOF'
#!/bin/sh
# Never-commit secret scan (code-plan C9) - installed by
# scripts/install-pre-commit-secret-scan.sh. Scans staged files and
# blocks the commit when secret material is found.
root=$(git rev-parse --show-toplevel)
cd "$root" || exit 1
if command -v python3 >/dev/null 2>&1; then
  exec python3 scripts/secret-scan.py --staged
else
  echo "pre-commit secret-scan: python3 not found, scan skipped" >&2
  exit 0
fi
HOOK_EOF
chmod +x "$hook"
echo "pre-commit secret-scan hook installed: $hook"
