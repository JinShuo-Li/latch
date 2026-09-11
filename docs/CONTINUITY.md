# Continuity Engine

> Context is disposable. Memory is durable.

Latch virtualizes context continuously. It does not wait for a full window,
summarize the transcript, discard originals, and continue from the summary. Each
request receives a newly materialized view: modular instructions, canonical task
state, durable memory with provenance, query-recalled original events, and a
budgeted tail of verbatim conversation.

1. **L0 active context:** recent verbatim dialogue, current results, recalled
   originals, and relevant code. It is disposable.
2. **L1 canonical state:** structured goal, constraints, decisions, hypotheses,
   files, validation state, questions, actions, and criteria.
3. **L2 episodic indexes:** event ranges and entities used to navigate older
   work. Descriptions point to raw sources.
4. **L3 raw store:** original durable events and artifacts. Normal context
   management never deletes them.

Summaries are indexes, not truth. Exact questions use deterministic SQLite FTS
to recover original events. Retrieval uses explicit relationships, paths,
entities, decisions, evidence, keywords, and recency. V0.1 has no embeddings.

Recent events are selected newest-first within the configured verbatim budget.
Canonical constraints and decisions are independently included. Older material
therefore leaves L0 without leaving durable memory. `/context` reports bytes by
category, reserve, event and episode counts, and health.

File observations include SHA-256 and size. A guarded mutation recomputes the
hash just before writing. A mismatch emits an external-change event and rejects
the edit so the model must re-read.

`/compact` is the sole compact action. It appends `manual_compact` and resets the
active working set while retaining raw events, canonical state, memory, and
evidence. There is no automatic compact event.

The stress test inserts an early constraint, decision, rejected approach, and
exact diagnostic, pushes them outside the recent budget, and verifies canonical
survival, exact raw recall, recent verbatim text, no automatic compact, and full
event retention.
