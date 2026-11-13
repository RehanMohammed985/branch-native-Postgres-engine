# Contributing

Issues and pull requests are welcome. The roadmap lives in the
[issue tracker](https://github.com/RehanMohammed985/branch-native-Postgres-engine/issues).

## Building and testing

Banyan needs Linux with FUSE 3 and PostgreSQL. On macOS, prefix commands with
`docker/dev.sh`, which runs them in a container that has everything.

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
tests/e2e.sh target/release/banyan     # after cargo build --release, as a non-root user
```

CI runs exactly these, plus a 1,000-seed crash simulation.

## Changes to the store

`banyan-store` is the part that can lose data, so it has the strictest rule:
a change to how data or metadata reaches the disk needs the crash simulation
to cover it.

- If you add an operation, add it to `tests/dst.rs` (both the store call and
  the reference model).
- Run more seeds than CI does before opening the pull request:
  `BANYAN_DST_SEEDS=5000 cargo test --release -p banyan-store --test dst`
- A failure prints its seed. `BANYAN_DST_SEED=<seed>` replays it exactly.
- If you can, show the simulation notices when your new invariant is broken:
  break it on purpose and check that a seed fails.

## Performance claims

Numbers in the README and `docs/BENCHMARKS.md` come from `bench/run.sh`. If a
change affects them, rerun it and update the tables and the JSON reports in
`docs/bench/` in the same pull request. State the losses as well as the wins.
