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

## Guided setup

`/setup` is the only place a user must configure a provider, its credential,
its models, and its thinking policy; a first run with no usable provider opens
it directly. The current wizard is linear — kind, name, endpoint, credential,
one model, one default effort — and persists only the provider row and
`[inference]`; typed model metadata is discarded. Guided setup replaces it with
a branch-first flow that defines several models per provider, separates the
display name from the request name, records the context window, maps thinking
levels onto the provider's wire values, and persists everything through the
canonical `[providers.*]` tables. One run configures one provider with any
number of models; repeating the flow configures more endpoints.

### Flow

```text
/setup
  Providers
    Add provider…
    Edit provider…          prefilled from persisted configuration
    Remove provider…        config only; stored credentials are kept
  Add provider
    OpenCode subscription
      Go                    https://opencode.ai/zen/go/v1
      Zen                   https://opencode.ai/zen/v1
        credential          environment variable name | masked secret
        models              multi-select from the service catalog
                            + "Refresh from provider…"
        per model           display name, context, thinking levels
        active model        becomes [inference].model
    Other provider
      endpoint type         OpenAI | Anthropic | DeepSeek | OpenAI-compatible
      instance id           stable config key, unique
      display name
      base URL              prefilled, editable
      credential            environment variable name | masked secret
      models                repeatable definition:
                              request name   wire model id sent on the wire
                              display name   what the TUI shows
                              context        tokens, optional
                              thinking       levels + wire mapping + default
      active model
  Review                    no secret is ever shown
  Apply & save              one atomic config write + credential store write
```

Keyboard: Up/Down move, space toggles in a multi-select, Enter advances,
Backspace/Esc goes back one step (and cancels at the first step). Every list
uses the windowed choice surface, so long catalogs scroll instead of clipping.

### Provider families

OpenCode Go and Zen are subscription services with published model lists and
per-model transports. Go keeps its versioned gateway URL,
`x-opencode-session` header, and per-model transport/replay semantics; setup
only changes how models are selected. Zen becomes a first-class kind
(`opencode-zen`, base `https://opencode.ai/zen/v1`), and its models resolve
transport per model like Go. Both branches list models from a built-in catalog
and can refresh live.

Other providers are the four existing kinds. The endpoint type fixes the
default transport and credential label; an OpenAI-compatible endpoint may
override the transport per model.

### Model definitions

The request name is the wire identity
(`[providers.<id>.models."<request>"]` and `[inference].model`); the display
name is presentation only. A definition carries the context window, the
exposed thinking levels, the wire mapping, the transport, replay policy, and
input modalities through the existing `ModelConfig` fields. Setup writes only
the fields it owns and preserves metadata for untouched models when editing.

### Thinking levels and wire mapping

The neutral vocabulary stays `none`/`minimal`/`low`/`medium`/`high`/`xhigh`/
`max`; `efforts` names the levels a model exposes and `default_effort` the
level a fresh selection starts on. When a provider does not speak those names,
`effort_map` maps each exposed level to its wire form:

```toml
[providers.acme.models."acme-pro"]
efforts = ["low", "medium", "high"]
default_effort = "medium"

[providers.acme.models."acme-pro".effort_map]
low = { value = "1" }
medium = { value = "2" }
high = { budget_tokens = 32768 }
```

Three wire forms, owned and validated by the transport adapter:

- `value = "…"` — the transport's documented effort field
  (`reasoning_effort`, `reasoning.effort`, `output_config.effort`,
  `thinkingConfig.thinkingLevel`).
- `budget_tokens = N` — the transport's documented token budget
  (`thinking.budget_tokens`, `thinkingConfig.thinkingBudget`).
- `disabled = true` — the transport's documented off switch
  (`thinking.type = "disabled"`, `thinkingConfig.thinkingBudget = 0`).

Rules: a map covers every exposed level or is absent (absent means the neutral
identity for transports that speak the neutral names); `default_effort` must be
one of `efforts`; budget and disabled forms are rejected for transports that
have no such field, with an actionable setup message; mapping is serialization,
so it lives in the adapter and the kernel loop and memory never see it.

The setup UI turns "how many levels" into a generated table. The user picks a
preset — neutral names, a numeric sequence, token budgets, or custom per level
— and the wizard creates the mapping automatically; every level stays editable,
and the review prints the mapping without any secret.

### Discovery

Go and Zen publish `GET /models` (public, and account-filtered when a
credential is entered). Built-in catalogs keep setup deterministic and
offline; a "refresh from provider" row asks the CLI to fetch and merge by id.
The TUI never performs network I/O: discovery is a request/response over the
existing channel (`Action::DiscoverModels` → `Output::SetupModels`), with a
timeout and a fall-back to the built-in list. Ids the catalog does not know
arrive with the id as label and conservative capabilities, and the per-model
editor fills in the rest.

### Zen and Gemini

Zen serves some models over Google's Generative Language API. Supporting them
adds a `Gemini` transport: streaming `generateContent` requests
(`models/{model}:streamGenerateContent?alt=sse`), `contents` with
`functionCall`/`functionResponse` parts, `inlineData` images, and
`thinkingConfig` for thinking. Gemini function calls carry no provider id, so
the adapter synthesizes deterministic per-turn call ids and maps tool results
by name; thinking has no replay requirement. Gateway auth shape and the exact
SSE chunk format are verified against the live endpoint before the adapter is
declared complete.

### Storage root and secrets

Everything Latch owns defaults to one root, `~/.latch/`, created `0700` on
first use:

```text
~/.latch/
  config.toml      profiles, models, policy; never a secret
  secrets.toml     provider API keys, 0600, atomic write
  latch.sqlite3    durable sessions and events
  artifacts/       session artifacts
```

Resolution stays explicit and predictable: an explicit `--config` path (or an
explicit `state_dir` in configuration) wins; otherwise `~/.latch/config.toml`
is the default. Existing installs keep working: when `~/.latch/config.toml`
does not exist but the legacy XDG files do (`~/.config/latch/config.toml`,
`~/.local/state/latch/`), Latch reads them in place and reports the legacy
root in `latch doctor`; `latch migrate` performs an explicit, non-destructive
move (config and secrets rewritten under `~/.latch/`, the session database
copied with SQLite's backup semantics, the legacy files left untouched as a
backup). No run silently relocates a user's sessions or credentials.

Secrets stay symbolic in configuration (`env:NAME`, `file:<provider>`). A
value typed in setup lives only in TUI memory until Apply, is masked in
display and debug output, is written atomically to `secrets.toml` under the
provider instance key with `0600`, and is refused on read if the file is
group- or world-readable. The whole root is masked inside the mandatory
sandbox, so sandboxed commands and extensions cannot read a key even when the
workspace or HOME changes. `latch doctor` reports the effective root and the
credential reference (never the value), and the setup review names the root it
will write to. `README.md`, `AGENTS.md`, and `config.example.toml` record the
same layout when this lands.

### Persistence and editing

`SetupPlan::Apply` carries the provider identity, its models, and the active
model. Apply upserts `[providers.<id>]`, every
`[providers.<id>.models.<request>]`, then `[inference]` once, and saves
atomically; a directly entered secret goes to the 0600 credential store under
the provider instance, and the config keeps only `env:`/`file:` references.
Editing prefills the flow from persisted configuration (credentials by
symbolic reference only) and preserves unrelated metadata. Removing a provider
never deletes stored credentials.

### Security and validation

Secrets exist only in TUI memory until Apply, are masked in display and debug
output, never enter `config.toml`, the event log, the transcript, or logs, and
are redacted from every error path. The storage root is `0700`, `secrets.toml`
is `0600` and written atomically, and every surface that names a credential
prints only its symbolic reference. Validation is explicit and actionable:
instance ids are unique and `[a-z0-9._-]`; URLs parse as http(s); environment
variable names are identifiers; secrets and request names are non-empty;
context windows are positive token counts; a provider has at least one model
and exactly one active model; a wire map is complete and transport-compatible.

### Phases

- **P0 — foundation:** the `~/.latch` default storage root (config, secrets,
  sessions, artifacts) with the legacy XDG read-in-place fallback and explicit
  `latch migrate`; multi-model `SetupPlan`; branch-first flow with edit;
  per-model definition (request/display/context); Zen kind for
  chat/responses/messages; built-in catalogs; validation; persistence; tests
  and user docs.
- **P1 — wire mapping:** `effort_map` config, adapter emission rules, and the
  mapping editor and review.
- **P2 — discovery:** the refresh round trip and catalog merge.
- **P3 — Gemini:** the transport adapter and Zen Gemini models.

Each phase is independently releasable, keeps the kernel's provider-neutral
memory and event types untouched, and updates `README.md`,
`config.example.toml`, and `ARCHITECTURE.md` where behavior or ownership
changes.
