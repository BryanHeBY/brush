# How to build and run

1. Install Rust toolchain. We recommend using [rustup](https://rustup.rs/).
1. Build `brush`: `cargo build`
1. Run `brush`: `cargo run`

The Reedline fork lives in the in-tree `brush-reedline` package. It is excluded
from the workspace so it can be published as its own crate; `brush-interactive`
consumes it by path in the source tree.
