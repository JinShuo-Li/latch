# Design

Latch keeps its default surface quiet: transcript, compact lifecycle rows, and a
bottom input. Detail is requested rather than continuously printed. Simple tasks
do not acquire ceremonial plans.

## Proportional effort

The system prompt is a small behavioral core, not a persistence manifesto. It
asks for the smallest amount of inspection, implementation, reasoning, and
validation that solves the actual task, and it stops at direct, relevant
evidence instead of searching for extra confidence, unrelated defects, or
cleanup. Simple local work needs no plan and a narrow check is sufficient
completion evidence for a narrow change; broader validation is reserved for
cross-cutting changes; planning is reserved for genuinely multi-step, ambiguous,
or long-horizon work; child agents are reserved for parallelizable, isolated
workstreams. The continue-until-done rule lives in exactly one prompt module,
so it never overrides the stopping rule. Kernel semantics are unchanged: the
model chooses strategy, the kernel owns facts. See
[`ARCHITECTURE.md`](ARCHITECTURE.md#prompt-architecture).

The model chooses investigation and debugging strategy. The kernel owns facts:
event order, file versions, process results, validation outcomes, evidence
provenance, policy decisions, mutations, cancellation, and durable state.
Prompts explain mechanisms while code enforces hard invariants. ASK and PLAN
cannot mutate (including validation commands that are not conservatively
read-only). A guarded edit targets a version Latch observed; drift that Latch
itself produced is recognized so repairs never demand a redundant re-read, while
genuine external modification is still refused. Forbidden commands do not
execute.

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

Verified completion is also what ends a run. An implementation claim is not a
substitute for evidence, so `ImplementedNotVerified` no longer terminates the
loop by itself: when the run actually changed the workspace and no required
validation has passed, the kernel spends one corrective turn asking the model to
validate the change or state why verification is unavailable. The state stays
honest either way, and the correction is one-shot — it can never loop, and it
can never promote an unverified run to Verified. Read-only work is untouched:
nothing was mutated, so there is nothing to verify.

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
artifact read, git status/diff, and conservative read-only shell observation by
canonical subject (including range arguments) and result digest inside a
progress epoch. Real change advances the epoch: workspace mutations, detected
external edits, validation and evidence, meaningful task-state updates, mode
switches, and new user turns. Repeating an unchanged observation once is
allowed; consecutive redundant turns cross the stagnation budget and the kernel
re-grounds the model with the explicit list of already-known observations, then
suppresses further repeats deterministically. Epoch and streak state replay
from raw events, so resume keeps an active inspection loop visible. There is no
turn ceiling: `failure.max_model_turns` is an optional, off-by-default circuit
breaker, and progress is bounded by real behavior, not task length.

## Mode, Safety, and Permissions

Mode, Safety, and Permissions are deliberately separate, because conflating them
makes the wrong thing adjustable. Mode is the shape of the work: ASK and PLAN
may inspect but never mutate; WORK may change the workspace according to Safety.
Safety is the risk posture: Strict asks before workspace writes, Standard treats
ordinary source edits as work, Autonomous removes that friction while still
classifying network and external effects. Permissions is how uncertainty is
resolved: a human decision, a recorded automatic approval, or a separate
stateless model review of the exact command. The kernel applies them in order;
none can widen what a previous stage refused.

The invariant that keeps provenance honest is that external effects are never
silently allowed. Writing outside the workspace, mutating `.git`, using the
network, or touching a remote system always becomes a kernel `Ask` first —
even when Safety is Autonomous and Permissions is All approved. Auto-approval
is a resolver, not a bypass: it records the same durable request/resolution
pair and issues a single-use capability grant for exactly that call. Hard deny
(privileged or system-destructive operations) sits outside all three controls
and can never be approved.

Enforcement is the OS sandbox, not bash-string parsing. `bwrap` is mandatory:
the host root is read-only, the workspace is mounted explicitly, `.git` is
protected unless Git metadata mutation was granted, home credentials and host
sockets are masked, and PID/IPC/UTS/network namespaces isolate the process.
Approval never means "rerun outside the sandbox"; it adds the narrowest
capability to a new sandbox for that call. The threat model is honest: this
strongly contains ordinary mistakes, prompt injection, accidental host access,
and unauthorized network use; it does not claim protection against kernel
exploits, resource exhaustion, or damage inside roots the user explicitly
granted.

## Context, reads, and approvals

Context is budgeted in tokens with a conservative estimator; every pre-request
number is visibly approximate and provider usage is authoritative afterward.
Tools are bounded by design rather than by small product ceilings: file reads
and artifact reads return continuation windows, searches return paged matches,
and long-running commands become managed processes with `exec_start` /
`exec_poll` / `exec_terminate`. `Ask` policy decisions pause for a real human
decision through the TUI; approval is single-use and keyed by kernel call ids
the model never sees, so consent cannot be fabricated. Outside-workspace writes
execute only after approval; dangerous shell commands remain denied.

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

## Transcript plus state

The transcript tells the user what the agent is doing; a responsive sidebar
tells the user what state the agent is in. Kernel truth remains the source of
status: working set, canonical task state, kernel-derived completion, evidence,
normalized usage categories, and change ownership. The UI never infers
verification or ownership from rendered prose, and never presents unknown
provider usage as zero. Abnormal states become visually obvious; healthy state
stays quiet.

Code modification reads at a glance: edit rows carry green `+N` and red `−N`
independently, a compact preview of the real unified diff is shown inline with
restrained tinted backgrounds on additions/removals, neutral unchanged context,
and dimmed hunk/file metadata, and `/diff` opens a typed unified-diff
inspector. Counts come from a real Myers line diff over the kernel change
ledger, so moved blocks and duplicate lines are not misreported. The preview
text is generated from the actual before/after bytes at mutation time, never
reconstructed from counters, and is bounded with an explicit omission note.
Only parsed unified diffs receive red/green semantics, so compiler output or
shell text can never be misclassified.

## Terminal experience

The composer is the application's control surface: a full-width neutral surface
band with a `>` prompt gutter, padding, a placeholder, mode/model/branch
metadata, and subdued hints. It steps its prompt accent down to gray when an
overlay or approval owns the screen. It is a real scrollable viewport over the
untouched buffer, so large pastes can be inspected at any position before
submission. An empty session shows a restrained centered identity instead of a
dead terminal.

One palette layer owns every surface, status tone, and diff tint, and adapts to
truecolor/ANSI-256/ANSI-16 and dark/light terminals; ANSI-16 drops backgrounds
rather than guessing the terminal palette. The transcript is typed and dense:
user messages on a neutral band with a `>` gutter, assistant messages on the
terminal background behind a quiet bullet and a deterministic Markdown subset,
one visual lifecycle row per tool call (running → done/FAIL, keyed by call id),
kernel notices, and errors. Transient activity lives in a compact status row
above the composer, not in the durable-looking transcript. Human approvals and
policy selection share one bottom action surface above the composer, keeping
the transcript visible; a full-request inspector is one keystroke away.
Hidden internals — reasoning content, context statistics, model usage, raw task
state — are never displayed, live or on resume. Typing `/` opens a command
palette filtered from the same list `/help` prints. The input is a real editor:
cursor motion, multiline (Ctrl+J or Alt+Enter), word kill, a bounded expanding
area, and
shell-like prompt history recalled with Up/Down that survives resume from user
events. Scrolling stays visual-row based with PageUp/PageDown, Home/End, mouse
wheel, auto-follow at the bottom, and a subtle newer-content indicator.
