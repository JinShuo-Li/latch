# Design

Latch keeps its default surface quiet: transcript, compact lifecycle rows, and a
bottom input. Detail is requested rather than continuously printed. Simple tasks
do not acquire ceremonial plans.

The model chooses investigation and debugging strategy. The kernel owns facts:
event order, file versions, process results, validation outcomes, evidence
provenance, policy decisions, mutations, cancellation, and durable state.
Prompts explain mechanisms while code enforces hard invariants. ASK and PLAN
cannot mutate (including validation commands that are not conservatively
read-only). A guarded edit cannot replace a version it did not observe.
Forbidden commands do not execute.

## Validation intent versus kernel truth

The model expresses what must hold and how to check it: `validate` takes a
semantic requirement name plus a command. The kernel runs the command, records
the result, links evidence provenance internally, and derives completion. The
model never needs event UUIDs, call ids, or ledger ids, and cannot write
validation truth: `record_evidence` accepts only `pending` and `unavailable`
observations, and `task_update` declares requirements — not pass states.

## Current evidence, honest completion

Evidence is a current-state ledger, not a history trap. The newest observation
per claim is current; the raw event log keeps every attempt. A test that failed
and now passes supersedes the failure, so a fixed task moves InProgress →
ImplementedNotVerified → Verified even with earlier failures behind it. A
required validation that cannot run yields Blocked rather than a pretense of
verification. `complete` only records the implementation claim; the kernel
computes the result and announces changes durably.

## Failure supervision

Failure signatures are normalized deterministically and tracked per validation
lineage. Unrelated successful tools (reading a file, searching, git status) do
not reset a failing validation's streak. The streak resolves when the failing
validation itself passes or the failure signature materially changes — evidence
the strategy moved. At the retry budget the kernel requests re-ground: inspect
reality, name disproven assumptions, change strategy. Supervision state
replays from raw events so resume keeps a stalled loop visible.

## Progress and stagnation

Successful inspections are supervised too. The kernel keys every read, search,
git status/diff, and conservative read-only shell observation by canonical
subject and result digest inside a progress epoch. Real change advances the
epoch: workspace mutations, detected external edits, validation and evidence,
meaningful task-state updates, mode switches, and new user turns. Repeating an
unchanged observation once is allowed; consecutive redundant turns cross the
stagnation budget and the kernel re-grounds the model with the explicit list of
already-known observations, then suppresses further repeats deterministically.
Epoch and streak state replay from raw events, so resume keeps an active
inspection loop visible, and the 32-turn limit remains only a last-resort
circuit breaker.

## Memory epistemics

Memory distinguishes user facts and constraints, task constraints, observations,
decisions, hypotheses, and model notes. Provenance and validity travel with each
record. Model-authored constraints are TaskConstraints; only real user messages
produce UserConstraints. Hypotheses can be supported, contradicted, or rejected;
they do not silently become observations, and a rejected hypothesis cannot be
re-added as a decision. Decisions, constraints, and questions can be superseded
or resolved explicitly; history stays in events and memory records while the
canonical view stays current.

## Change ownership

Latch records the dirty starting tree and retains pre-edit bytes for Latch- and
shell-owned changes as content-addressed artifacts, so ownership survives
restart. Undo verifies the current file is still the recorded post-change
version, then restores only that change; it refuses after external modification
and never drops the record on refusal. Shell commands that mutate the workspace
are classified as shell-originated where detection is possible (Git worktrees,
bounded snapshots) and marked explicitly non-reversible or undetectable
otherwise — Latch never pretends a shell mutation pre-existed. Destructive Git
recovery, automatic commit, and automatic push are absent.

## Terminal experience

The transcript is typed and dense: user messages, assistant messages rendered
with a small deterministic Markdown subset, one visual lifecycle row per tool
call (running → done/FAIL, keyed by call id), kernel notices, and errors.
Hidden internals — reasoning content, context statistics, model usage, raw task
state — are never displayed, live or on resume. Typing `/` opens a command
palette filtered from the same list `/help` prints. The input is a real editor:
cursor motion, multiline (Alt+Enter), word kill, a bounded expanding area, and
shell-like prompt history recalled with Up/Down that survives resume from user
events. Scrolling stays visual-row based with PageUp/PageDown, Home/End, mouse
wheel, auto-follow at the bottom, and a subtle newer-content indicator.
