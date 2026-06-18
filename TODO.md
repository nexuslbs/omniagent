# TODO — Improve Context Grounding, Memory, and MCP Extensibility

## 1) Context Builder (Selective, Ranked Prompt Assembly)

- [x] Create a `ContextBuilder` pipeline before each LLM call in `agent::process_message`.
- [x] Assemble prompt context from ordered blocks:
  - [x] System/profile instructions
  - [x] `MEMORY.md` (always include, size-capped by user config; default < 5000 chars)
  - [x] Recent thread messages (recency window)
  - [ ] Last user messages (pinned)
  - [x] Retrieved past messages (relevance-ranked via ILIKE)
  - [ ] Retrieved wiki snippets (relevance-ranked)
  - [ ] Allowed tool definitions only
- [x] Add token budgeting per block (reserve output tokens, trim lowest-priority blocks first).
  - [x] Never trim: System/profile instructions
  - [x] Never trim: `MEMORY.md`
- [x] Persist context assembly metadata in `messages.metadata` (selected message IDs, wiki files, token counts).

## 2) Retrieval Strategy (Past Messages + Wiki)

- [x] Implement hybrid retrieval for historical context:
  - [x] Semantic retrieval (pgvector / embeddings)
  - [x] Keyword fallback (ILIKE / lexical)
- [x] Reflect project defaults in retrieval sources:
  - [x] postgres messages (objective facts, focus on recent conversations)
  - [x] pgvector over messages (subjective/semantic facts over objective message facts)
  - [x] wiki (long-term objective facts)
  - [x] qdrant over wiki (vectorized wikis)
- [x] Add re-ranking step favoring:
  - [x] Recency
  - [x] Same thread/channel
  - [x] User-confirmed facts
- [x] Add retrieval guardrails:
  - [x] Max snippets per source type
  - [x] Per-snippet char/token cap
  - [ ] Dedup by semantic similarity

## 3) Hallucination Reduction / Grounding Policy

- [x] Update system/profile prompt policy:
  - [x] Prefer retrieved evidence over prior assumptions
  - [x] If uncertain, explicitly state uncertainty
  - [x] For factual/project-specific claims, provide grounding references
- [x] Add internal evidence structure in metadata for each final answer:
  - [x] `context.selected_message_ids[]` (message IDs)
  - [x] `context.wiki_files[]` (file paths/sections)
  - [ ] `evidence.tools[]` (tool call IDs)
- [x] Add low-confidence fallback behavior:
  - [x] Ask clarifying question, or
  - [x] Trigger retrieval/tool call before answering
- [ ] Add contradiction check between drafted answer and retrieved evidence.

## 4) Memory Model (Short-Term vs Long-Term)

- [x] Keep full raw history in `messages` table (all roles/types) as source of truth.
- [x] Introduce explicit long-term memory promotion workflow to wiki:
  - [x] Promote only validated/repeatedly useful facts
  - [x] Store provenance (source message IDs / tool outputs)
  - [x] Store confidence and `last_verified_at`
- [x] Add review/expiry workflow for long-term memory entries.
- [x] Keep `MEMORY.md` user-authored and always-included (size-capped by user config).
- [ ] Evaluate adding an episodic/hindsight memory layer for relationship-over-time summaries with provenance and confidence.

## 5) “Remember to Retrieve” Behavior

- [x] Add question classifier (fast heuristic/model): decide when retrieval is required.
- [x] Auto-trigger `search_messages` / `search_wiki` for factual or repo-specific queries.
- [x] Add profile-level knobs:
  - [x] `auto_retrieval_enabled`
  - [x] `retrieval_aggressiveness`
  - [x] `grounding_required`

## 6) MCP Runtime Hardening (Current Built-in Tools)

- [ ] Enforce strict JSON Schema validation for tool inputs.
- [ ] Add per-tool timeout/retry policy and error taxonomy.
- [ ] Add idempotency and side-effect classification (`read_only`, `mutating`, `external_network`).
- [ ] Require confirmation gate for high-risk mutating tools.
- [ ] Improve observability:
  - [ ] Persist tool latency, success/failure class, retry count
  - [ ] Link tool call/result records with stable IDs

## 7) MCP Extensibility (Add Tools Without Binary Release)

- [ ] Design external MCP server integration:
  - [ ] Transport support: `stdio` first, HTTP/SSE next
  - [ ] Capability negotiation + tool discovery
- [ ] Add dynamic tool registry layer:
  - [ ] Merge built-in + external tools at runtime
  - [ ] Profile-level allowlist enforcement across both
- [ ] Add secure secret handling for external tool auth.
- [ ] Add health checks / circuit breaker per external MCP server.

## 8) Data/Schema & Telemetry Enhancements

- [x] Extend `messages.metadata` schema conventions for:
  - [x] context selection diagnostics
  - [x] evidence references
  - [ ] confidence score
- [ ] Add tables (or JSON schema) for:
  - [ ] feedback signals (explicit/implicit)
  - [ ] wiki memory provenance and verification status
- [ ] Add periodic metrics jobs:
  - [ ] Groundedness rate
  - [ ] Retrieval hit rate
  - [ ] Tool success rate
  - [ ] Hallucination proxy metrics (user corrections/re-asks)

## 9) Evaluation Loop (Continuous Improvement)

- [ ] Build eval dataset from real conversations + expected outcomes.
- [ ] Add regression suite for profile/model/prompt changes.
- [ ] Track quality/cost/latency per profile and model.
- [ ] Block prompt/profile rollouts on eval regressions.

## 10) Rollout Plan

- [x] Phase 1: Context builder + grounding policy + metadata evidence logging.
- [x] Phase 2: Hybrid retrieval + auto-retrieval trigger + contradiction checks.
- [x] Phase 3: Memory promotion workflow + provenance + review cycle.
- [x] Phase 4: MCP external servers + dynamic tool registry.
- [ ] Phase 5: Full eval/feedback-driven optimization.

## 11) Documentation Updates

- [x] Update `AGENTS.md` with:
  - [x] Context assembly rules
  - [x] Grounding/citation policy
  - [ ] Memory promotion criteria
  - [ ] MCP extension model (built-in vs external)
- [ ] Add operator runbook for tuning retrieval and grounding settings.
