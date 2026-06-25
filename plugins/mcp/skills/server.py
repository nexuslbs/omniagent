#!/usr/bin/env python3
"""skills MCP server — create_skill tool for reusable task procedures."""

import os
import re
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "sdk"))
from server import McpServer

server = McpServer(name="skills", version="0.1.0")

DATA_DIR = os.environ.get("OMNI_DATA_DIR", "/opt/data")


def _validate_skill_name(name: str) -> str | None:
    if not name:
        return "Skill name must not be empty"
    if len(name) > 64:
        return f"Skill name must be 64 characters or less (got {len(name)})"
    if not re.match(r'^[a-z0-9_-]+$', name):
        return "Skill name must match pattern: lowercase alphanumeric, hyphens, underscores only"
    return None


@server.tool(
    name="create_skill",
    description="Create a new skill (SKILL.md file) for reusable procedures. Skills allow the agent to automate recurring task patterns. The skill is saved to <data_dir>/skills/<category>/SKILL.md and will be available for future sessions.",
    input_schema={
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Skill name (lowercase, hyphens/underscores, max 64 chars)",
            },
            "description": {
                "type": "string",
                "description": "Brief description of what the skill does",
            },
            "content": {
                "type": "string",
                "description": "Full markdown body of the skill (steps, verification, etc.)",
            },
            "category": {
                "type": "string",
                "description": "Optional category for organizing (e.g., 'devops', 'data-science'). Default: 'general'",
            },
        },
        "required": ["name", "description", "content"],
    },
)
def handle_create_skill(arguments):
    name = arguments.get("name", "")
    description = arguments.get("description", "")
    content = arguments.get("content", "")
    category = arguments.get("category", "general")

    # Validate
    err = _validate_skill_name(name)
    if err:
        return (f"Error: {err}", True)
    if not description:
        return ("Error: Skill description must not be empty", True)
    if not content:
        return ("Error: Skill content must not be empty", True)

    # Normalize name
    normalized = name.lower().replace(" ", "-")

    # Determine skill directory
    skills_dir = os.path.join(DATA_DIR, "skills", category)
    skill_path = os.path.join(skills_dir, f"{normalized}.md")

    try:
        os.makedirs(skills_dir, exist_ok=True)

        # Build SKILL.md with frontmatter
        skill_content = f"""---
name: {normalized}
description: "{description}"
version: 0.1.0
author: omniagent
---

{content}
"""
        with open(skill_path, "w") as f:
            f.write(skill_content)

        return (f"Created skill at {skill_path}", False)
    except Exception as e:
        return (f"Error creating skill: {e}", True)


if __name__ == "__main__":
    server.run()
