#!/usr/bin/env python3
"""Replace -> Result< with -> AppResult< in function signatures across src/ files.

For files that only use our custom Result type (not std::result::Result or anyhow::Result),
this is safe. For files with mixed usage, we need to be careful.

Files processed:
- Pure Result files: all fn return types changed from Result< to AppResult<
- Mixed files: only targeted replacements
"""
import re
import os

SRC_DIR = '/opt/workspace/omniagent/src'

# Files where ALL Result< in return position should be changed to AppResult<
# These files exclusively use our custom Result type
PURE_FILES = [
    'db/kanban.rs',
    'db/threads.rs',
    'db/mod.rs',
    'commands.rs',
    'actions.rs',
    'agent/config.rs',
    'agent/executor.rs',
    'hindsight_populator.rs',
    'llm/mod.rs',
    'mcp/external/client.rs',
    'mcp/external/config.rs',
    'mcp/mod.rs',
    'mcp/tools/actions.rs',
    'platform/external/client.rs',
    'platform/external/mod.rs',
    'platform/mod.rs',
    'plugin/installer.rs',
    'plugin/mod.rs',
    'plugins_yaml.rs',
    'relevance.rs',
    'main.rs',
]

# Files with mixed usage - need selective replacement
MIXED_FILES = {
    # scheduler.rs: has Result<(), String> (std) and our custom Result types
    'scheduler.rs': [
        # Replace all -> Result< that are our custom type (not followed by , or _)
        # Pattern: -> Result<...> where ... does NOT contain a comma
    ],
    # server/mod.rs: has Result<CreateActionRequest, _> (std) + Result<()> (ours)
    'server/mod.rs': [],
    # vectorizer/mod.rs: has Result<Self> (FromStr) + our custom Result types
    'vectorizer/mod.rs': [],
    # agent/mod.rs: has Result<MsgRow, _> (std) + crate::error::AppResult<u64> (already done)
    'agent/mod.rs': [],
    # subtask/mod.rs: uses anyhow::Result in fn sigs + imports AppResult (but doesn't use it)
    'subtask/mod.rs': [],
    # db/types.rs: has Result<Self, Self::Error> (TryFrom) + crate::error::AppResult<...> (already done)
    'db/types.rs': [],
}

def replace_in_file(filepath, old, new):
    """Simple string replacement in file."""
    with open(filepath, 'r') as f:
        content = f.read()
    if old in content:
        content = content.replace(old, new)
        with open(filepath, 'w') as f:
            f.write(content)
        return True
    return False

# Process pure files - replace `-> Result<` with `-> AppResult<` in function signatures
for fname in PURE_FILES:
    filepath = os.path.join(SRC_DIR, fname)
    if not os.path.exists(filepath):
        print(f"SKIP (not found): {fname}")
        continue
    
    with open(filepath, 'r') as f:
        content = f.read()
    
    # Replace `-> Result<` with `-> AppResult<` 
    # But NOT inside other things - only at function return types
    new_content = content.replace('-> Result<', '-> AppResult<')
    
    # Also handle closures: `|...| -> Result<McpToolResult>` 
    # These should already be covered by the above pattern
    
    if new_content != content:
        with open(filepath, 'w') as f:
            f.write(new_content)
        print(f"UPDATED: {fname}")
    else:
        print(f"NO CHANGE: {fname}")

# Process mixed files
# agent/mod.rs: has `let specific: Result<MsgRow, _>` which should NOT be changed
filepath = os.path.join(SRC_DIR, 'agent/mod.rs')
with open(filepath, 'r') as f:
    content = f.read()
# Only change -> Result< not Result< in let bindings
new_content = content.replace('-> Result<', '-> AppResult<')
if new_content != content:
    with open(filepath, 'w') as f:
        f.write(new_content)
    print(f"UPDATED: agent/mod.rs (selective)")
else:
    print(f"NO CHANGE: agent/mod.rs")

# scheduler.rs: has `Result<(), String>` which is std::result::Result - don't change
# But change our custom Result types
filepath = os.path.join(SRC_DIR, 'scheduler.rs')
with open(filepath, 'r') as f:
    content = f.read()
# Don't change `Result<(), String>` but change other `-> Result<`
# The pattern `-> Result<(), String>` should not change
# We use a careful approach: change `-> Result<` but NOT `-> Result<(),`
new_content = content.replace('-> Result<', '-> AppResult<')
# Fix the overreplacement:
new_content = new_content.replace('-> AppResult<(), String>', '-> Result<(), String>')
new_content = new_content.replace('Result<(), String>', 'Result<(), String>', 1)  # Already fixed
# Actually check what we have
if new_content != content:
    with open(filepath, 'w') as f:
        f.write(new_content)
    print(f"UPDATED: scheduler.rs (selective)")
else:
    print(f"NO CHANGE: scheduler.rs")

# server/mod.rs: has `Result<CreateActionRequest, _>` which is std - don't change
filepath = os.path.join(SRC_DIR, 'server/mod.rs')
with open(filepath, 'r') as f:
    content = f.read()
new_content = content.replace('-> Result<', '-> AppResult<')
# Fix the overreplacement of the std Result:
new_content = new_content.replace(
    'let result: AppResult<CreateActionRequest, _>',
    'let result: Result<CreateActionRequest, _>'
)
if new_content != content:
    with open(filepath, 'w') as f:
        f.write(new_content)
    print(f"UPDATED: server/mod.rs (selective)")
else:
    print(f"NO CHANGE: server/mod.rs")

# vectorizer/mod.rs: has `fn from_str(s: &str) -> Result<Self>` (FromStr trait) 
# which is std::result::Result - DON'T change
filepath = os.path.join(SRC_DIR, 'vectorizer/mod.rs')
with open(filepath, 'r') as f:
    content = f.read()
new_content = content.replace('-> Result<', '-> AppResult<')
# Fix overreplacement of FromStr trait impl:
new_content = new_content.replace(
    'fn from_str(s: &str) -> AppResult<Self>',
    'fn from_str(s: &str) -> Result<Self>'
)
if new_content != content:
    with open(filepath, 'w') as f:
        f.write(new_content)
    print(f"UPDATED: vectorizer/mod.rs (selective)")
else:
    print(f"NO CHANGE: vectorizer/mod.rs")

# subtask/mod.rs: uses anyhow::Result, has AppResult imported but doesn't use it
# No changes needed for function signatures (they already use anyhow::Result)
print(f"SKIP: subtask/mod.rs (uses anyhow::Result)")

# db/types.rs: TryFrom impls use Result<Self, Self::Error> which is std
# The crate::error::Result<Vec<...>> was already changed to AppResult
print(f"SKIP: db/types.rs (already done - TryFrom uses std::result::Result)")

print("\nDone!")
