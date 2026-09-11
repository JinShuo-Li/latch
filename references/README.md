# Reference source

Latch was designed after studying Pi and OpenAI Codex as public product and
engineering references. Latch is an independent implementation: neither source
tree is vendored nor a runtime or build dependency.

The exact reviewed snapshots are recorded in `pi.rev` and `codex.rev`.
Materialize them with `scripts/fetch-references.sh`; the checkouts are shallow,
ignored by Git, and made read-only.
