# Dogfood experiment log

An empirical log of autonomous self-dogfooding on `dev` using the Docker
harness (`docs/DOGFOOD_DOCKER.md`) and a real OpenCode Go provider. Each
iteration: inspect behavior and evidence, name one bottleneck, state a
falsifiable hypothesis, make the smallest change, run the full validation
(`cargo fmt`, `clippy`, `cargo test --workspace`, `scripts/dogfood-test.sh`),
run a real-provider dogfood task, and keep the change only if evidence
supports it.

The harness, fixtures, and configs live outside the product. No credential is
recorded here or committed anywhere.

## Environment

- Host: Raspberry Pi 5 (aarch64), Docker 29.8.0.
- Provider: OpenCode Go, model `deepseek-v4-flash`, machine mode (`latch run
  --output json`), safety `standard`, permissions `human`.
- Fixture: a buggy `calc.py` (`add` returns `a - b`) with an existing unittest;
  prompt: "Fix the bug in calc.py so the existing unittest passes. Use the
  project's own test command to verify the fix."
- Egress note: the host reaches the internet through a TUN-based transparent
  proxy that does not capture container bridge traffic; a host-side CONNECT
  proxy on the bridge gateway was used to let the container run the real
  provider. That proxy is test infrastructure only and is not part of Latch.

## Baseline (commit e391633, before any iteration)

| Metric | Value |
| --- | --- |
| exit | 1 |
| status | failed |
| completion | in_progress |
| model turns / events | 12 |
| input/output tokens | unknown (no model response) |
| elapsed | 4 s |
| error | `HTTP 404` from the provider endpoint |

## Iteration 1 - OpenCode Go default endpoint (accepted, c843d0a)

**Bottleneck.** Real-provider dogfooding could not start: every request
returned HTTP 404 before any model turn.

**Hypothesis.** The default `opencode-go` base URL omits the `/v1` path
segment, so `{base}/chat/completions` addresses a non-existent route.

**Evidence (falsifiable).** Unauthenticated probes:

```
POST https://opencode.ai/zen/go/chat/completions      -> 404
POST https://opencode.ai/zen/go/v1/chat/completions   -> 401
GET  https://opencode.ai/zen/go/v1/models             -> 200
```

An authenticated POST to `/v1/chat/completions` returned
`400 MissingSessionID`, confirming the route exists and that Latch's existing
`x-opencode-session` header (already attached for OpenCode Go endpoints) is
the remaining requirement.

**Change.** `ProviderKind::OpenCodeGo::default_base_url()` now returns
`https://opencode.ai/zen/go/v1`. Endpoint detection stays a prefix match on the
bare gateway root, so the stable session header and model selection are
unchanged. `config.example.toml` updated. Regression test added.

**Result.** The same real dogfood run now reaches kernel-derived
`completion=verified` with a correct workspace fix instead of failing with
404.

## Iteration 2 - read-only `.git` filters (accepted, ab18999)

**Bottleneck.** The iteration-1 run ended `status=permission_denied` (exit 3)
even though the task reached `completion=verified`. Durable events showed the
first tool call, a read-only
`ls -la && find . -name "*.py" -not -path "*/.git/*"`, was classified as
`GitMetadataWrite` and denied (`source=non_interactive`).

**Hypothesis.** The command classifier flags any token equal to `.git` or
containing `.git/`, so a `.git` path used only as a read-only exclusion pattern
is misclassified as a metadata mutation.

**Change.** `inferred_command_capabilities` now ignores `.git` tokens that are
arguments to read-only filters (`-path`, `-not`, `--glob`, `--exclude`,
`--exclude-dir`, `--ignore`, …). Direct `.git` targets, redirections into
`.git/`, and Git write verbs still classify. The sandbox keeps `.git`
read-only regardless, so this narrows an Ask heuristic, not the enforcement
boundary.

**Evidence.** Deterministic regression tests assert read-only filters are
allowed under Standard/Autonomous and metadata mutations are still classified.
The real run of the same task now ends exit 0 / `completed` / `verified`.
Caveat: that run happened to open with a plain `ls`, so the exit-0 result is
consistent with the fix but not solely caused by it; the deterministic tests
and the iteration-1 reproduction are the primary evidence.

**Measured effect (same prompt and fixture).**

| Metric | Baseline | Iter 1 | Iter 2 |
| --- | --- | --- | --- |
| exit | 1 | 3 | 0 |
| status | failed | permission_denied | completed |
| completion | in_progress | verified | verified |
| events | 12 | 95 | 79 |
| input tokens | - | 40180 | 33294 |
| output tokens | - | 730 | 587 |
| cache read tokens | - | 33280 | 26368 |
| cache miss tokens | - | 2081 | 2110 |
| result file correct | no | yes | yes |

## Iteration 3 - read-only Git queries (accepted, ed73507)

**Bottleneck.** A purely read-only task ("report branch, last commit, and
configured Git user.name/user.email; modify nothing") exited 3. Durable events
showed `git config user.name`, `git config --get user.email`, and
`git config user.email; echo "email_lookup_exit=$?"` denied as Git metadata
writes; the model then spent turns working around the denials.

**Hypothesis.** Only the first `git` verb in a command is classified, and
command separators are dropped during tokenization, so (a) later `git`
invocations are judged by the first verb, and (b) trailing shell words are
absorbed as arguments of the preceding `git config`. Read-only `config`
queries are therefore classified as writes.

**Change.** Tokenization now keeps a `;` boundary token for shell control
operators, and every `git` invocation is classified separately. `git config`
queries (`--get`, `--get-all`, `--get-regexp`, `--list`, `-l`, `--show-origin`,
…) and a single bare key (`git config user.name`) are read-only; mutating flags
(`--add`, `--unset`, `--replace-all`, `--edit`, …), `key=value`, and
two-argument forms still classify. All-flag listing forms for
branch/remote/tag/stash no longer absorb trailing commands. The sandbox keeps
`.git` read-only regardless.

**Result.**

| Metric | Before (iter3 probe) | After (iter3 fixed) |
| --- | --- | --- |
| exit | 3 | 0 |
| status | permission_denied | completed |
| events | 99 | 32 |
| input tokens | 52862 | 16051 |
| output tokens | 2961 | 492 |
| cache miss tokens | 2086 | 606 |

## Iteration 4 - system prompt trimming (accepted, see commit)

**Bottleneck.** The compiled system prompt was ~1489 estimated tokens: verbose
prose plus a `[id vN]` header on all 16 fragments sent to the provider every
request.

**Hypothesis.** Dropping the provider-facing fragment headers and tightening
prose-only fragments cuts per-request token spend without losing any behavioral
rule, and the smaller stable prefix does not harm the prefix cache.

**Change.** Fragment content was tightened and the `[id vN]` headers were
removed from the provider-facing text (ids/versions remain compiler metadata
shown by `latch debug prompt`). Every rule asserted by the prompt tests is
preserved; the budget test was lowered from 1380/1520 to 1295/1305 tokens and
now also asserts the headers are gone.

**Result.**

| Metric | Baseline | Iter 4 (matched 79 events) |
| --- | --- | --- |
| compiled prompt tokens | 1489 | 1298 (-12.8%) |
| per-request `request_tokens` | 6806 | 6589 (-3.2%) |
| aggregate input tokens | 33361 | 32446 (-2.7%) |
| provider `cache_read/input` | 0.798 | 0.797 |
| task success | verified | verified |

**Cache-read proportion.** Trimming the stable prefix is ratio-neutral by
construction: per-request, the cached prefix and the request both shrink by the
same amount. The durable usage events show the first request is entirely
uncached (~4.6k tokens) and every later request is already ~91-94% cached, so
the aggregate ~0.80 is dominated by the first request plus small per-turn
misses. Raising the aggregate proportion requires shrinking per-turn volatile
content or the number of turns, not the system prompt; that is recorded as the
remaining bottleneck.

## Rejected / no-change experiments

- **Host-side CONNECT proxy** was needed to run the real provider from a
  container in this specific network. It is not a Latch change and stays
  outside the repository.
- A first decision-level regression test asserted the `.git`-filter command is
  `Allow` under **Strict**; that is wrong because Strict asks for every shell
  workspace write by design. The test was narrowed to Standard/Autonomous (the
  machine-dogfood default) while still asserting no `GitMetadataWrite`
  capability under Strict.

## Remaining bottlenecks

- Cache-read proportion is prefix-bound: the first request is always fully
  uncached and later requests are already ~91-94% cached. The lever is per-turn
  volatile content (tool output, state deltas, narration) or fewer turns, not
  the stable system prompt.
- `cat .git/HEAD` or `cat .git/config` are still classified as Git metadata
  writes and ask for approval: the `.git` path scan does not distinguish
  read-only readers (`cat`, `head`, `ls`) from writers. Fixing that needs a
  broader read/write command classifier and was not attempted.
- Any genuinely unresolved Ask still marks an otherwise verified run as
  `permission_denied` (exit 3) by design; the changes removed false positives,
  not the fail-closed policy.
- The runtime image has no language toolchains beyond Python 3; Rust/Node/Go
  dogfood tasks need a derived image.
- Real-provider runs depend on host egress and are slower and more variable
  than the deterministic mock path; wall-clock timings here are not comparable
  across runs.
