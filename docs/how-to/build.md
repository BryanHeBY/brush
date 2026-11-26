# How to build and run

1. Install Rust toolchain. We recommend using [rustup](https://rustup.rs/).
1. Build `brush`: `cargo build`
1. Run `brush`: `cargo run`

The Reedline fork lives in the in-tree `brush-reedline` package. It is excluded
from the workspace so it can be published as its own crate; `brush-interactive`
consumes it by path in the source tree.

## Maintaining the Reedline vendor

Use the last recorded upstream base and the requested target for the source
range. [Reedline source](../../brush-reedline/UPSTREAM.md)

Set `scratch` to an absolute
temporary directory and `base` and `head` to the full commit IDs. Fetch
upstream `main` over HTTPS with TLS verification, then verify
the range before editing.

```sh
git -c http.sslVerify=true clone \
    --single-branch --branch main --no-tags https://github.com/nushell/reedline.git "$scratch/upstream"
git -C "$scratch/upstream" rev-parse refs/remotes/origin/main
git -C "$scratch/upstream" merge-base --is-ancestor "$head" refs/remotes/origin/main
git -C "$scratch/upstream" merge-base --is-ancestor "$base" "$head"
git -C "$scratch/upstream" rev-list --count "$base..$head"
git -C "$scratch/upstream" log --reverse --oneline "$base..$head"
git -C "$scratch/upstream" diff --find-renames --name-status "$base" "$head"
git -C "$scratch/upstream" diff --find-renames --binary --full-index "$base" "$head" > "$scratch/reedline.patch"
```

Compare the old upstream tree with the current vendor after mapping upstream
paths under `brush-reedline/`. From the isolated Brush checkout, use a scratch
clone to see where a three-way application would merge or conflict. Set
`brush_base` to the Brush commit you are updating.

```sh
git clone --shared --no-checkout "$PWD" "$scratch/brush"
git -C "$scratch/brush" switch --detach "$brush_base"
GIT_ALTERNATE_OBJECT_DIRECTORIES="$scratch/upstream/.git/objects" \
    git -C "$scratch/brush" apply --3way --directory=brush-reedline "$scratch/reedline.patch"
```

Review the scratch result path by path and reconcile it in the vendor. Account
for additions, removals, renames, and paths omitted from the vendor. Compare
the result against the new upstream tree to identify the remaining Brush
patches. Review manifests, features, MSRV, lockfile implications, and license
notices. Record skipped and adapted changes with their rationale and update
the source base in `UPSTREAM.md` when the sync is complete. Keep the archive
record and package identity tied to their releases.

Check each upstream revision's formatter pin or CI toolchain, rustfmt config,
and Cargo edition in its own checkout. If upstream CI floats, record the exact
version used to reproduce its baseline. Run upstream formatting checks only
in the corresponding upstream checkout. Brush uses edition 2024 from its
root `rustfmt.toml` and the exact toolchain from `rust-toolchain.toml`. Format
only the changed in-tree Rust files with that pin, then check both manifests.

```sh
rustfmt --edition 2024 --config-path rustfmt.toml --config skip_children=true path/to/changed.rs
cargo fmt --all -- --check
cargo fmt --manifest-path brush-reedline/Cargo.toml --all -- --check
cargo clippy --manifest-path brush-reedline/Cargo.toml --all-targets --all-features --jobs 2 -- -D warnings
cargo clippy --locked --workspace --all-targets --all-features --jobs 2 -- -D warnings
```

On Linux, use `TERM=xterm-256color` with `NO_COLOR` unset for the terminal
assertions. Run the vendor tests and the shell's PTY tests. The latter drive
the Brush executable, including replay, Readline bindings, history navigation,
highlighting, and repainting. Set `BASH_PATH` to an installed Bash executable
to include the optional behavioral comparisons.

```sh
cargo test --manifest-path brush-reedline/Cargo.toml --jobs 2 --lib
cargo test --locked --jobs 2 -p brush-shell --test reedline_interactive_tests --test pty_startup_tests
```

Verify the package that goes through release before publishing
`brush-reedline` ahead of `brush-interactive`.

```sh
cargo package --manifest-path brush-reedline/Cargo.toml --jobs 2
```

Run the platform-enabled PTY tests on macOS as well. On native Windows run the
vendor tests and start `cargo run --locked --jobs 2 -p brush-shell -- --input-backend=reedline`
in a console. Paste a multiline command with CRLF, move the caret between
lines, and shorten the input to confirm that repaint clears stale text. Use
`cargo check --locked --jobs 2 -p brush-shell --target x86_64-pc-windows-gnu`
from a host with the Windows GNU target installed for a cross-compile check.
