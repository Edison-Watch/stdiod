# Contributing to stdiod

Thanks for your interest in improving stdiod! This is early-stage, experimental
software, so the contribution process is intentionally lightweight.

## Getting started

You'll need a [Rust toolchain](https://rustup.rs/). The channel and components
are pinned in [`rust-toolchain.toml`](./rust-toolchain.toml), so `rustup` will
install the right versions automatically.

```sh
git clone https://github.com/Edison-Watch/stdiod.git
cd stdiod
cargo build --workspace
```

## Before you open a pull request

Please make sure the same checks CI runs pass locally:

```sh
cargo fmt --all --check                                   # formatting
cargo clippy --workspace --all-targets -- -D warnings     # lints (warnings are errors)
cargo test --workspace                                    # tests
cargo build --workspace                                   # build
```

A one-shot equivalent:

```sh
cargo fmt --all --check && \
  cargo clippy --workspace --all-targets -- -D warnings && \
  cargo test --workspace
```

## Guidelines

- **Keep changes focused.** Small, single-purpose PRs are easier to review.
- **Match the surrounding style.** Follow the existing naming, comment density,
  and module layout; `cargo fmt` handles formatting.
- **Update docs alongside code.** If you change the wire protocol, update
  [`schema/tunnel-protocol.json`](./schema/tunnel-protocol.json) (the single
  source of truth) and [`ARCHITECTURE.md`](./ARCHITECTURE.md). If you change the
  CLI or config, update [`README.md`](./README.md).
- **Add tests** for new behavior where practical.
- **Describe the why.** A short explanation of the motivation and approach in
  the PR description goes a long way.

## Reporting bugs and security issues

- For ordinary bugs and feature requests, open a
  [GitHub issue](https://github.com/Edison-Watch/stdiod/issues).
- For **security vulnerabilities**, do **not** open a public issue — follow
  [`SECURITY.md`](./SECURITY.md) instead.

## License

By contributing, you agree that your contributions will be licensed under the
[GNU Affero General Public License v3.0](./LICENSE) that covers this project.
