# Contributing to headroom-hook

Thanks for your interest. This is a small, standalone Rust binary — a Busbar
[hook](https://getbusbar.com/docs/hooks/) that compresses chat history with
[headroom-core](https://github.com/headroomlabs-ai/headroom).

## Ground rules

- Be respectful and constructive (see [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)).
- By contributing, you agree your contributions are licensed under the
  project's [Apache-2.0](LICENSE) license.

## Build and test

```sh
cargo build --release      # the shipped binary
cargo test                 # unit + wire tests
cargo clippy --all-targets -- -D warnings   # lints must be clean
cargo fmt --all            # format before committing
```

The core (`src/compress.rs`, `src/main.rs`) stays lean; tests live in
`src/tests/`. Keep it that way — a hook is read by the people deciding whether
to trust it on their request path.

## Before a PR

1. `cargo fmt --all` — rustfmt-clean.
2. `cargo clippy --all-targets -- -D warnings` — no warnings.
3. `cargo test` — green.
4. If you touch the wire behavior, add or update a test in `src/tests/` that
   pins it.
