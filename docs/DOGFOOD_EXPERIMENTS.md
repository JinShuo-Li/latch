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

- Reads of `.git` (`cat .git/HEAD`) are still classified as metadata writes and
  ask for approval; distinguishing read-only from mutating commands would need
  a broader, riskier classifier change.
- Any genuinely unresolved Ask still marks an otherwise verified run as
  `permission_denied` (exit 3) by design; the fix removed a false positive, not
  the fail-closed policy.
- The runtime image has no language toolchains beyond Python 3; Rust/Node/Go
  dogfood tasks need a derived image.
- Real-provider runs depend on host egress and are slower and more variable
  than the deterministic mock path.
