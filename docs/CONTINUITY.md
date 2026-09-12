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
3. protocol-safe recent working memory: the largest suffix of whole
   conversation units (an assistant tool-call turn and all of its results are
   one unit) whose estimated size fits `recent_tokens`; a single oversized unit
   is kept whole rather than truncated;
4. targeted recalled original events;
5. the scored episode index, capped at 16 entries;
6. tool schemas and extension context, reserved by the agent before the
   continuity engine allocates its own sections.

Recalled originals and the episode index are estimated together, so recalled
material is never double counted; `episode_tokens` distinguishes the archival
index from recalled originals inside `recall_tokens`. `/context` reports the
token breakdown, the first retained sequence, how many tokens left working
memory since the previous request, reserve, headroom, event and episode counts,
and status.

## Gradual working-memory decay

Working memory does not collapse at a threshold. Each new turn appends whole
units and, when the recent budget is exceeded, only the oldest units needed to
fit are evicted — one at a time in the common case. The retained tail is a pure
function of the durable log, the budget, and the last `/compact`, so resume
reconstructs exactly the same working set without any rollover bookkeeping.
Tool transactions stay atomic, and an oversized transaction is kept whole
rather than split. Evicted units are never deleted: they move into the archival
region, remain searchable through FTS, and stay visible through the episode
index. `/compact` remains the only explicit working-memory reset; legacy
`ContextEpochStarted` events from older builds are retained for history but no
longer move the retained start.

Context changes are explainable from durable state: every materialization emits
`ContextMaterialized` with `recent_tokens`, `recent_start_sequence`, and
`recent_evicted_tokens`, so an advancing window start and the tokens it
released can be reconstructed from the event log alone.

## Incremental history access

Every event stays in the raw store; only the queries are incremental.
Materialization loads recent candidates from the newest events with
`events_tail`, computes the retained tail locally, and asks the episode cache
for only the newly archived delta via `events_between`; it never deserializes
history it has already indexed. Recall uses deterministic SQLite FTS and
fetches only matched rows, preserving the original match limit before excluding
bookkeeping kinds. Live supervision keeps sequence cursors, so each turn feeds
only newly appended events to the progress supervisor and the live sink, and
resume continues from the durable cursor.

## Episodes

Episodes segment on user intents (a new user message starts a new episode, with
a volume cap) rather than fixed chunks. Each episode carries its event range,
topic, file entities, tool names, and structural markers (`validation-passed`,
`validation-failed`, `reground`, `mutation`, `evidence`, `failure`). A
deterministic score — lexical overlap with the query, current file entities,
marker weights, and recency — selects a bounded subset for the index.

The index is built incrementally. A cached builder keeps the closed episodes
and the open trailing segment plus a sequence watermark; new events extend the
open segment, and only a session change or a working-memory budget increase
(which moves the archive backwards) triggers a deterministic rebuild from the
raw log. The one-shot builder and the streaming builder share the same
accumulation rules, and equivalence tests pin that incremental extension equals
a full rebuild at every prefix. Exact raw-event recall stays available via FTS,
and no episode ever replaces the events it summarizes.

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
