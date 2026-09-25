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

## Configuration and setup

`/setup` is the configuration center: viewing, adding, editing, and removing
providers, credentials, models, and provider defaults. `/model` is fast live
switching only — it changes the running session's provider, model, and effort
and never edits configuration. A normal user configures a provider in four
choices plus Save:

```text
Provider -> Credential -> Models -> Default model -> Save
```

Nothing else is required to get a known provider working. Context window,
transport, reasoning replay, input modalities, effort wire mapping, token
budgets, and aliases live under Advanced and are never part of the default
path. Built-in provider and model facts come from the Latch catalog; user
configuration stores selections and overrides, never a copy of the built-in
metadata.

### Configuration center

`/setup` opens on the provider list, not a step-by-step wizard. Each row shows
what the user needs to decide next:

- display name and stable provider id;
- status: `ready`, `missing credential`, or `unresolved models`;
- configured model count;
- the provider's default model.

A first run with no usable provider opens the same list with Add highlighted.
The list is the navigation hub for per-provider surfaces: Credential, Models,
Default model, and Advanced, plus Add provider, Edit provider, and Remove
provider. Every list uses the windowed choice surface, so long catalogs scroll
instead of clipping.

```text
/setup
  Providers                     ready · missing credential · unresolved
    Add provider…
      OpenCode Go | OpenCode Zen | OpenAI | Anthropic | DeepSeek | Custom Provider
        Credential              environment variable name | masked secret
        Models                  catalog multi-select, add custom model, refresh
        Default model
        Save
    Edit provider…
      Credential / Models / Default model / Advanced
    Remove provider…            config only; stored credentials are kept
```

### Simplified paths and Advanced

Known providers — OpenCode Go, OpenCode Zen, OpenAI, Anthropic, and DeepSeek —
resolve base URL, transport, reasoning replay, input modalities, context
window, and effort mapping from the catalog. Their setup asks only for a
credential, the models to enable, and a default model. Only Custom Provider
asks for protocol and base URL by default; transport, replay, modalities,
context, aliases, and wire mapping stay under Advanced.

Adding a custom model normally asks only for the request/model id and a
display name. Everything else about it — transport, context window, replay,
modalities, effort mapping, token budgets — is Advanced, and a model with an
unresolved transport is not offered for activation until it is filled in.

### Metadata resolution

Effective model metadata resolves in exactly one order:

```text
builtin catalog -> discovered metadata -> user overrides
```

- **Builtin catalog** is Latch-owned truth for known providers and models:
  transport, context window, effort set and default, replay policy, and input
  modalities. It ships with Latch and is never copied into `config.toml`.
- **Discovered metadata** is availability evidence only: an id the provider
  serves. It may mark a known catalog model as available, and it may introduce
  an unknown id, but it never invents transport or capability facts.
- **User overrides** win field by field and are the only layer that can resolve
  an unknown model's transport or capabilities.

`config.toml` therefore contains provider identity, credentials as symbolic
references, model selections, `default_model`, and the override fields a user
actually set — not a second catalog.

### Models, selections, and defaults

- The catalog defines the known model set for a provider. Custom models extend
  it through overrides.
- `providers.<id>.enabled_models` optionally selects which models the provider
  offers; absent means the full catalog plus custom entries. Selections are
  not metadata.
- `providers.<id>.models.<request>` holds per-model overrides only, and is
  written only when a user changes a field. Existing override entries are
  preserved field by field.
- `providers.<id>.default_model` is the model selected when switching to that
  provider.
- `[inference]` is the default profile for a new session.
- The current session's `/model` choice is live runtime state, durable for
  that session and never written back to configuration.

These are three different defaults and none may stand in for another:

1. `providers.<id>.default_model` — the model `/model` selects when it
   switches to that provider. It must not fall back to catalog index 0.
2. `[inference]` — the provider/model/effort a fresh session starts with.
3. The running session's `/model` selection — live state only.

Save persists the provider; it updates `[inference]` only when no session
default exists or the user explicitly chooses Set as session default. `/model`
never rewrites `[inference]`, a provider default, or an override.

### Thinking strength

The provider-neutral effort vocabulary stays `none`/`minimal`/`low`/`medium`/
`high`/`xhigh`/`max`. Built-in models map efforts to the wire automatically
through their transport adapter; the default flow only chooses which levels to
expose and which one is the default.

Wire mapping is a Custom/Advanced concern. `effort_map` maps each exposed level
to the transport's documented form:

```toml
[providers.acme.models."acme-pro"]
efforts = ["low", "medium", "high"]
default_effort = "medium"

[providers.acme.models."acme-pro".effort_map]
low = { value = "1" }
medium = { value = "2" }
high = { budget_tokens = 32768 }
```

- `value = "…"` — the transport's effort field (`reasoning_effort`,
  `reasoning.effort`, `output_config.effort`, `thinkingConfig.thinkingLevel`).
- `budget_tokens = N` — the transport's token budget
  (`thinking.budget_tokens`, `thinkingConfig.thinkingBudget`).
- `disabled = true` — the transport's documented off switch
  (`thinking.type = "disabled"`, `thinkingConfig.thinkingBudget = 0`).

A map covers every exposed level or is absent (absent means the neutral
identity for transports that speak neutral names); `default_effort` must be
one of `efforts`; budget and disabled forms are rejected for transports
without such a field, with an actionable setup message. Mapping is
serialization: it lives in the adapter, and the kernel loop and memory never
see it.

### Discovery

Go and Zen publish `GET /models` (public, and account-filtered when a
credential is entered). Built-in catalogs keep setup deterministic and
offline; a refresh row asks the CLI to fetch and merge by id. The TUI never
performs network I/O: discovery is a request/response over the existing
channel (`Action::DiscoverModels` → `Output::SetupModels`), with a timeout and
a fall-back to the built-in catalog.

Discovery is conservative:

- an id in the response proves only that the provider serves that id;
- known catalog models merge dynamic availability with their built-in
  metadata, and the catalog metadata still wins for transport and
  capabilities;
- unknown ids are marked `unresolved` for transport and capabilities, appear
  only in `/setup`, and require Advanced configuration before they can be
  activated; `/model` never presents one as ready;
- discovery never overwrites a user override, and a failed or empty response
  leaves configuration and the built-in catalog untouched.

### Zen and Gemini

Zen serves some models over Google's Generative Language API. Supporting them
adds a `Gemini` transport: streaming `generateContent` requests
(`models/{model}:streamGenerateContent?alt=sse`), `contents` with
`functionCall`/`functionResponse` parts, `inlineData` images, and
`thinkingConfig` for thinking.

The transport must not assume the API omits call ids: provider-native call ids
are preserved whenever they are present, and deterministic ids are synthesized
only as a compatibility fallback for APIs or models that genuinely omit them.
Tool responses are associated by call id, with the function name used only as
a sanity check, never as the sole correlation key.

Thought signatures are reasoning state, not decoration: Gemini thought and
function-call parts can carry signatures that must be preserved and replayed
when the API requires them for multi-turn reasoning. Before P3 lands, the
durable reasoning representation is reassessed for positional thought parts:
the current provider-neutral artifacts must be able to store every thought
part, its order, and its opaque signature losslessly, or the representation is
extended. A design that drops or reorders thought signatures is not
acceptable; replay is capability metadata, exactly as it is for every other
transport.

### ResolvedPaths and the storage root

One abstraction, `ResolvedPaths`, owns path resolution; no other module calls
XDG or default-path APIs, and no other module constructs a config, state,
secret, database, artifact, or cache path independently. It resolves:

- `config_path`
- `state_root`
- `secrets_path`
- `database_path`
- `artifacts_root`
- `cache_root`
- the path source (`explicit`, `new`, or `legacy`) and the legacy source paths
  when one applies.

The new default layout is a single root:

```text
~/.latch/
  config.toml      profiles, models, selections, overrides; never a secret
  secrets.toml     provider API keys, 0600, staged writes
  latch.sqlite3    durable sessions and events
  artifacts/       session artifacts
  cache/           regenerable metadata: discovery snapshots, catalogs
```

`cache/` holds only regenerable data and is safe to delete. The root is
created `0700` on first use. An explicit `--config` path wins; an explicit
`state_dir` in configuration overrides `state_root`. Otherwise
`~/.latch/config.toml` is the default. `latch doctor` reports every resolved
path and its source, and the setup review names the root it will write to.

### Legacy compatibility and `latch migrate`

Existing XDG installations keep working without a rewrite: when
`~/.latch/config.toml` does not exist and a legacy config does
(`~/.config/latch/config.toml`, with state under `~/.local/state/latch/`),
`ResolvedPaths` resolves `legacy` and Latch reads those files in place. No run
silently relocates sessions or credentials.

Migration is explicit and one-directional. `latch migrate` rewrites the
configuration under the new root, stages the secrets there, copies the session
database with SQLite backup semantics, copies artifacts, writes a migration
marker recording the source paths and timestamp, and renames the legacy
`config.toml` (and any legacy secrets file) to a non-discoverable backup name.
After migration:

- deleting `~/.latch/config.toml` yields fresh defaults and a fresh setup; it
  must not silently reactivate the stale legacy configuration;
- backups may be kept, but migrated backups no longer participate in automatic
  discovery, because discovery matches only the exact live filenames;
- the marker keeps `latch doctor` able to explain where the installation came
  from.

### Crash-consistent persistence

Configuration and secrets are separate files and are not updated as one
cross-file transaction. Saves are staged and ordered so a crash can never
produce configuration that references a secret which was never committed:

1. validate the whole change first; a validation failure writes nothing;
2. write temporary files in the target directory;
3. commit `secrets.toml` first (fsync, rename);
4. commit `config.toml` last (fsync, rename).

Failure semantics are defined rather than assumed: a crash before the secrets
commit leaves the previous configuration intact; a crash after the secrets
commit can leave an orphan secret, which is acceptable because nothing
references it; a crash can never leave a `file:<id>` reference without its
committed secret. Removing a provider writes configuration first and may leave
an orphan secret by design. On load, a `file:<id>` reference with no stored
value is reported as `missing credential` — a recoverable provider status, not
a startup failure — and `/setup` offers to enter the key again.

### Security and validation

Secrets exist only in TUI memory until Save, are masked in display and debug
output, never enter `config.toml`, the event log, the transcript, or logs, and
are redacted from every error path. The storage root is `0700`, `secrets.toml`
is `0600` and staged, a secrets file that is group- or world-readable is
refused on read, and the whole root is masked inside the mandatory sandbox so
sandboxed commands and extensions cannot read a key even when the workspace or
HOME changes. Every surface that names a credential prints only its symbolic
reference; deleting a stored secret is an explicit action, never a side effect
of removing a provider.

Validation is explicit and actionable: provider ids are unique and
`[a-z0-9._-]`; URLs parse as http(s); environment variable names are
identifiers; secrets and request names are non-empty; context windows are
positive token counts; a provider has at least one enabled model and a
`default_model` that is enabled; an override never widens a capability the
transport cannot express; and unresolved models cannot be activated.

### Phases

- **P0a — paths, storage, migration, permissions:** `ResolvedPaths`, the
  `~/.latch` layout, `cache/`, legacy read-in-place discovery, the migration
  marker and non-discoverable backups, `latch migrate`, and the `0700`/`0600`
  permission rules.
- **P0b — config domain and persistence:** multi-model provider config,
  selections vs overrides, the three defaults, and crash-consistent staged
  saves with defined recovery.
- **P0c — configuration center:** the `/setup` provider list with status,
  Add/Edit/Remove, simplified known-provider paths, Custom Provider basics,
  Advanced disclosure, and `/model` as live switching only.
- **P0d — Zen with existing transports:** the `opencode-zen` kind for
  chat/responses/messages models, with built-in catalogs and no Gemini yet.
- **P1 — effort mapping:** `effort_map` config, adapter emission rules, and
  the Custom/Advanced mapping editor.
- **P2 — discovery:** the conservative refresh round trip, catalog merge, and
  `unresolved` handling.
- **P3 — Gemini:** the transport adapter with native call ids, call-id
  correlation, and lossless thought-signature replay, including any
  provider-neutral reasoning-representation extension it requires.

Each phase is independently releasable, keeps the kernel's provider-neutral
memory and event types intact, and updates `README.md`,
`config.example.toml`, and `ARCHITECTURE.md` where behavior or ownership
changes.
