# Offline eval and regression baseline

The eval harness scores the committed JSONL corpus through the real write and
recall paths. It uses the deterministic in-process hash embedder, so it needs
no provider, network, or model access.

## Run the gate

```sh
make eval
```

The target:

1. creates a throwaway database configured with `[embed] provider = 'hash'`;
2. runs every case in `fixtures/eval_cases.jsonl`;
3. prints precision, recall, MRR, leaks, and case count; and
4. compares the result with `fixtures/eval_baseline.json`.

The absolute floors remain independent of the baseline:

- precision >= 0.90
- recall >= 0.95
- MRR >= 0.80
- forbidden-id leaks == 0

The committed baseline is a second guard. Quality metrics may drop by at most
`0.01` (one percentage point) below the stored value. A corpus-size change or
a new leak is always a failure. On a drift failure, the command prints both
`baseline:` and `measured:` lines and exits with status 2.

## Review or update a baseline

A normal `make eval` never writes the artifact. To deliberately replace it:

```sh
AGOS_EVAL_UPDATE_BASELINE=1 make eval
```

The update is written only after the absolute floors pass. It records:

- the release label (`v0.6.0` via the Makefile);
- the short git commit supplied by the Makefile;
- the corpus size;
- the `0.01` tolerance; and
- the measured metrics.

Review the resulting `fixtures/eval_baseline.json` diff as part of the change.
Do not update it merely to make a failing run green. The update environment
variable and release metadata are explicit so an operator can reproduce the
artifact-generation command.

## Determinism and regression tests

`tests/eval_gate.rs` runs the shipped corpus twice and compares the complete
metrics, ensuring the baseline cannot flake due to provider or database state.
It also perturbs a fixture query and verifies the unchanged baseline check
fails with both baseline and measured values. A synthetic `Metrics` test covers
the exact tolerance boundary and the absolute-floor check remains independently
covered.

Run the focused suite directly with:

```sh
cargo test --test eval_gate -- --nocapture --test-threads=1
```

The release evidence, including the current baseline values, is recorded in
[`docs/proof.md`](proof.md).
