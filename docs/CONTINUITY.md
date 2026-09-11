# Continuity Engine

> Context is disposable. Memory is durable.

Latch virtualizes context continuously. It does not wait for a full window,
summarize the transcript, discard originals, and continue from the summary. Each
request receives a newly materialized view: modular instructions, canonical task
state, current evidence and failure lineages, durable memory with provenance,
query-recalled original events, and a budgeted tail of verbatim conversation.

1. **L0 active context:** recent verbatim dialogue, current results, recalled
   originals, and relevant code. It is disposable.
2. **L1 canonical state:** structured goal, constraints, decisions, hypotheses,
   files, validation requirements, current evidence, failure lineages,
   questions, actions, and criteria.
3. **L2 episodic indexes:** scored, bounded entries over intent-aligned event
   ranges used to navigate older work. Descriptions point to raw sources.
4. **L3 raw store:** original durable events and artifacts. Normal context
   management never deletes them.

Summaries are indexes, not truth. Exact questions use deterministic SQLite FTS
to recover original events. Retrieval uses explicit relationships, paths,
entities, decisions, evidence, keywords, and recency. There are no embeddings
and no vector database.

## Bounded materialization

Context budgets are tokens, not bytes. Bytes remain only for internal file,
artifact, log, and I/O limits.

Latch estimates tokens with a conservative, provider/model-aware estimator.
ASCII text is priced at roughly four characters per token, punctuation at two,
and wide non-ASCII characters (CJK, full-width forms, emoji) at two for
OpenAI/Anthropic profiles and one for CJK-optimized models. Every user-visible
number is approximate (`≈`) until the provider reports real usage; provider
usage is authoritative.

The complete request obeys a hard invariant:

    instructions + state + recent + recall + tools + extension
        <= context_window_tokens - reserve_tokens

The context window defaults to 256k tokens and is overridable per model with
`[models.<name>] context_window_tokens`; `reserve_tokens` covers the model's
output plus safety. Components are allocated in explicit priority order:

1. hard system/kernel instructions (the compiled prompt);
2. canonical core — goal, constraints, decisions, required validations,
   current evidence, active failure lineages (memory lines drop
   lowest-priority first: notes before hypotheses before decisions);
3. protocol-safe recent verbatim transcript (tool transactions stay atomic —
   an assistant tool-call turn and all of its results are selected as one
   unit, never split at a budget boundary) within `recent_tokens`;
4. targeted recalled original events;
5. the scored episode index, capped at 16 entries;
6. tool schemas and extension context, reserved by the agent before the
   continuity engine allocates its own sections.

Recalled originals and the episode index are estimated once as one block, so
recalled material is never double counted. `/context` reports the token
breakdown, reserve, headroom, event and episode counts, and status.

## Episodes

Episodes segment on user intents (a new user message starts a new episode, with
a volume cap) rather than fixed 20-event chunks. Each episode carries its event
range, topic, file entities, tool names, and structural markers
(`validation-passed`, `validation-failed`, `reground`, `mutation`, `evidence`,
`failure`). At materialization time a deterministic score — lexical overlap
with the query, current file entities, marker weights, and recency — selects a
bounded subset for the index. Exact raw-event recall stays available via FTS.

## File state and compaction

File observations include SHA-256 and size. A guarded mutation recomputes the
hash just before writing. A mismatch emits an external-change event and rejects
the edit so the model must re-read.

`/compact` is the sole compact action. It appends `manual_compact` and resets
the active working set while retaining raw events, canonical state, memory, and
evidence. There is no automatic compact event.

## Stress guarantees

The stress tests insert an early constraint, decision, rejected hypothesis, and
exact diagnostic, then push thousands of events through the session. They
verify canonical survival of constraints and decisions, rejection state
preserved (a rejected hypothesis never renders as active memory), exact raw
recall of an early diagnostic, recent verbatim text, the bounded-token
invariant under thousands of durable events, exact component sums (no double
counting), a bounded episode index, no automatic compact, and full raw event
retention.
