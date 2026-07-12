# Plan: Extract Wiki & Context Assembly from Core to Plugins

## Problem

Core omniagent currently hardcodes knowledge about:
- Wiki directory structure (`{data}/profiles/{profile}/wiki/`)
- Text-based wiki file search
- Wiki embedding generation (HashVectorizer)
- Wiki vectorization (background worker)
- Wiki search result format (path, title, content)

All of this should be plugin concerns. The agent should ask "give me relevant context" via MCP tools, not walk filesystems and call vector databases directly.

## Architecture

```
┌─────────────────────────────────────────────┐
│  omniagent core                              │
│                                              │
│  context_builder.rs                          │
│    no wiki logic                              │
│    calls tools: search_wiki, search_memory    │
│    via MCP tool calls (not hardcoded)         │
│                                              │
│  vectorizer/mod.rs                           │
│    removed entirely                          │
└─────────────────────────────────────────────┘
         │ MCP tool calls
         ▼
┌─────────────────────────────────────────────┐
│  memory/search plugin                        │
│                                              │
│  knows about:                                 │
│    - wiki dir structure                       │
│    - Qdrant connection                       │
│    - embedding models                        │
│    - text + semantic search                  │
│                                              │
│  provides tools:                              │
│    - search_wiki(query, limit, profile)       │
│    - search_messages(query, limit)            │
│    - vectorize_wiki(path, content)            │
└─────────────────────────────────────────────┘
```

## Phase 1: Define Plugin Interface (MCP Tools)

### Tool: `search_context`
- Input: `{ query: string, limit?: number, profile?: string }`
- Output: `{ results: [{ title, content, source, score }] }`
- The plugin decides which sources to search (wiki, memory, messages)
- Core calls this once per context assembly; plugin handles all retrieval

### Tool: `vectorize_content`
- Input: `{ content_type: "wiki" | "messages", items: [{ id, content, metadata }] }`
- Output: `{ status: string, count: number }`  
- Plugin handles embedding + storage

## Phase 2: Remove Wiki Knowledge from context_builder.rs

### Current flow (context_builder.rs):
1. Build context blocks for: skills, parent thread, wiki, memory
2. Wiki block: `search_wiki_text()` walks wiki dir + (was) Qdrant semantic search

### New flow:
1. Build context blocks for: skills, parent thread, **search results**, memory
2. **Search results block**: call `search_context` MCP tool
   - If tool not registered → skip (plugin not installed → no context enrichment)
   - If tool responds → inject results into context

### Changes needed:
- Remove `search_wiki_text()` call from `context_builder.rs`
- Remove wiki dir path construction
- Remove `HashVectorizer` usage
- Add MCP tool call for `search_context` at appropriate priority
- Remove `db/types.rs` functions: `search_wiki`, `search_wiki_text`, `search_wiki_qdrant`

## Phase 3: Remove vectorizer/mod.rs

### Current:
- `VectorizerWorker` runs background tasks for messages + wiki
- Wiki vectorizer reads AgentConfig for Qdrant URL, API keys, etc.

### New:
- Delete `MessageVectorizer`, `WikiVectorizer`, `VectorizerConfig`, `VectorizerWorker`
- The plugin handles its own vectorization scheduling (via cron or its own loop)
- Core no longer has any vectorization code

### Changes needed:
- Remove `vectorizer/` module entirely
- Remove `start_vectorizer()` from main.rs
- Remove vectorization config fields from `AgentConfig` (VECTORIZE_MESSAGES, VECTORIZE_WIKI, MESSAGES_VECTORIZATION_*, WIKI_VECTORIZATION_*)
- Remove vectorization settings from `settings.rs`

## Phase 4: Remove Qdrant References from db/types.rs

### Current:
- `search_wiki_qdrant()` — Qdrant HTTP API call
- `search_wiki()` — orchestrates text + Qdrant search

### New:
- Delete both functions (they're only called from context_builder.rs, which will use MCP tools instead)

## Phase 5: Clean Up Settings & Config

### Remove from settings definitions:
- All vectorization env vars (VECTORIZE_MESSAGES, VECTORIZE_WIKI, MESSAGES_VECTORIZATION_*, WIKI_VECTORIZATION_*)

### Remove from AgentConfig:
- All vectorization fields

## Implementation Order

1. **Phase 2** (context_builder) — core change, biggest impact
   - Add MCP tool call mechanism to context_builder
   - Remove wiki text search
   - Remove HashVectorizer
   - Done: core no longer knows about wiki paths or search

2. **Phase 1** (plugin interface) — define the `search_context` tool
   - Create a memory/search plugin with `search_context` tool
   - Plugin handles wiki text + Qdrant search
   - Plugin owns the wiki directory walking and embedding logic

3. **Phase 3** (vectorizer removal) — remove background vectorization from core
   - The plugin implements its own vectorization (via cron or timer)
   - Core no longer spawns vectorizer workers

4. **Phase 4+5** (cleanup) — remove dead db functions, config fields, settings
   - Delete `db/types.rs` search functions
   - Remove vectorization from AgentConfig and settings definitions

## Risks

1. **Context quality regression** — if `search_context` tool is slow or unavailable, context assembly degrades. Solution: graceful fallback (skip search results block, agent still works without enrichment).

2. **Plugin development cost** — the memory/search plugin needs to replicate what core currently does (wiki text search, Qdrant integration). But this is the right separation — omni-stack provides the plugin, core stays clean.

3. **Scheduling** — vectorization currently runs as a background tokio task. The plugin would need its own scheduling mechanism (Hermes cron job, internal timer, etc.).

## Success Criteria

- [ ] `context_builder.rs` has zero wiki-related code
- [ ] `vectorizer/` module deleted from core
- [ ] No `QDRANT_URL`, `VECTORIZE_*`, `WIKI_*` env vars in core code
- [ ] `db/types.rs` has no wiki search functions
- [ ] All wiki logic lives in a plugin
- [ ] 123/123 unit tests still pass
