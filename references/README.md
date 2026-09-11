# Reference source

Latch was designed after studying Pi's public source as a product and engineering
reference. Latch is an independent implementation: Pi is neither vendored nor a
runtime or build dependency.

The exact reviewed snapshot is recorded in `pi.rev`. Materialize it with
`scripts/fetch-references.sh`; the checkout is shallow, ignored by Git, and must
remain read-only.
