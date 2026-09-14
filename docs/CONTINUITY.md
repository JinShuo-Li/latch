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

Every child agent has its own complete L0-L3 stack. Parent history does not seed
a child's active context: it starts from a compact delegation brief and the
workspace repository instructions. Child task state and evidence remain local
to its session. A compact semantic `AgentReport` is the only result transferred
back; its validation entries are informational and never become parent
evidence.

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

## Cache epochs and working memory

Working memory is organized into durable cache epochs. The governing rule is
that memory decides what the model needs to know, while the cache decides how
cheaply it can be sent: cache epochs are performance boundaries, not memory
boundaries.

Within an epoch the provider-visible conversation is append-only. Kernel-owned
context is persisted as durable `KernelContext` messages rather than a
synthetic tail: a complete authoritative snapshot starts the epoch, and deltas
are appended only when state actually changes — a full current-state update at
a higher revision, extension sources, recalled originals, an archival index
update, or a re-ground instruction. Revision numbers make supersession
explicit, and messages from older generations are excluded from the
provider-visible epoch while remaining durable. Ordinary task-state,
evidence, or validation changes therefore append instead of rewriting earlier
messages, and resume replays the same kernel history.

Asynchronous child lifecycle events are hidden from recent context and recall.
The root supervisor buffers completed reports, and the root loop appends an
`AgentNotificationDelivered` event only at a safe model boundary after every
earlier tool call has a terminal result. That delivered event is semantic,
provider-visible, and a durable dedupe marker. It therefore extends the current
cache epoch normally without rewriting its prefix or exposing child transcripts
and kernel bookkeeping.

`recent_tokens` is the conversation high-water mark. When an epoch exceeds it,
one hysteretic rotation retains the newest whole semantic units up to about
three quarters of the budget and emits a fresh snapshot. The quarter-budget of
headroom makes rotation occasional rather than per-turn; tool transactions stay
atomic and an oversized unit is kept whole. Rotation never deletes memory:
evicted material stays in the raw log, remains searchable through FTS recall,
and stays visible through the episode index. `/compact` is the explicit
working-memory reset and starts a fresh epoch.

Canonical state remains authoritative. A higher kernel revision is current
truth; the append-only history is provenance, and old decisions, constraints,
or evidence never remain ambiguously current because each state update carries
the complete current canonical state. Recall reaches into the archival region
only for material that is no longer in the provider-visible epoch.

## Context metrics

Three distinct quantities are reported, never conflated:

- **estimated architecture cacheability**: `common_prefix_tokens /
  request_tokens`, measured on Latch's canonical serialization and token
  estimator; an architecture diagnostic only;
- **provider prefix utilization**: `cache_read_tokens / common_prefix_tokens`,
  how much of the estimated reusable prefix the provider actually read;
- **measured provider cache hit rate**: `cache_read_tokens / (cache_read_tokens
  + cache_miss_tokens)` from provider-reported categories; unknown categories
  remain unknown.

Every materialization also records the cache epoch generation, its
conversation span in user turns, the last rotation reason, and the tokens
retained after that rotation, so context changes are explainable from durable
events alone.

Image inputs participate honestly: `image_count` and `image_tokens` report how
many images are visible in the epoch and their estimated visual tokens
(computed from dimensions, never from base64 length). The image estimate is a
partition of `recent_tokens`, provider-reported usage remains authoritative
after the request, and an image with unknown dimensions uses a conservative
fallback rather than zero. Because durable history stores only a `MediaRef`,
rotation and resume replay the exact immutable artifact, not the current
contents of the original path.

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

## Group coordination durability

Agent-group events are ordinary durable events in the root session. Tasks,
claims, membership, and queued messages survive resume exactly like the rest of
history; a claim is committed by the same immediate transaction that appends
its event, so a restart never loses or duplicates ownership. The SQLite group
projection is a rebuildable cache over those events and is reconstructed at
store open; `GroupState::replay` yields the same tasks, dependencies, claims,
membership, and delivery state from the same ordered events.

Group messages are information-only. A queued message stays pending until its
recipient reaches a safe model boundary, where delivery appends
`GroupMessageDelivered` to the recipient's own durable session together with
its delivery marker in one transaction. FIFO per recipient and exactly-once
across restarts are properties of the log, not of process memory, and an idle
agent is never woken by information alone. Messages are not evidence and never
merge child contexts: delivered text becomes one compact user-role observation.

The `/group` view and the sidebar GROUP block reduce the same events, so live
and resumed presentation agree.

## Stress guarantees

The stress tests insert an early constraint, decision, rejected hypothesis, and
exact diagnostic, then push thousands of events through the session. They
verify canonical survival of constraints and decisions, rejection state
preserved (a rejected hypothesis never renders as active memory), exact raw
recall of an early diagnostic, recent verbatim text, the bounded-token
invariant under thousands of durable events, exact component sums (no double
counting), a bounded episode index, no automatic compact, and full raw event
retention.

Stress tests are local by convention and stay out of CI. CI runs only the
small, fast architectural invariant tier in
`crates/latch-kernel/tests/invariants.rs`: durable history ownership, append-only
cache epochs, canonical authority, resume equivalence, kernel-owned evidence,
safety hard-deny, steering protocol correctness, deterministic serialization,
and cache-accounting semantics.
