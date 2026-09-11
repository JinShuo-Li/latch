# Design

Latch keeps its default surface quiet: transcript, compact lifecycle rows, and a
bottom input. Detail is requested rather than continuously printed. Simple tasks
do not acquire ceremonial plans.

The model chooses investigation and debugging strategy. The kernel owns facts:
event order, file versions, process results, policy decisions, mutations,
cancellation, and durable state. Prompts explain mechanisms while code enforces
hard invariants. ASK and PLAN cannot mutate. A guarded edit cannot replace a
version it did not observe. Forbidden commands do not execute.

Session history is an append-only event stream with stable UUID identity,
monotonic session sequence, time, and parent identity. Provider messages are
materialized from history and are never authoritative. Task state is structured:
goal, constraints, decisions, hypotheses, rejected hypotheses, touched files,
validations, questions, next actions, criteria, and completion.

Memory distinguishes user facts and constraints, observations, decisions,
hypotheses, and model notes. Provenance and validity travel with each record.
Hypotheses can be supported, contradicted, or rejected; they do not silently
become observations.

Completion is evidence-based. An implementation claim without successful
required validation remains `IMPLEMENTED_NOT_VERIFIED`. Failure signatures are
normalized deterministically. At the retry budget the kernel emits a re-ground
request asking the model to inspect reality and change strategy.

Latch records the dirty starting tree and retains pre-edit bytes for Latch-owned
changes. Undo verifies the current file is still the Latch-written version, then
restores only that change. It refuses after external modification. Destructive
Git recovery, automatic commit, and automatic push are absent.
