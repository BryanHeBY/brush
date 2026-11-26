# Reedline source

## Provenance

| Record | Value |
| --- | --- |
| Source repository | [nushell/reedline](https://github.com/nushell/reedline) |
| crates.io archive | `reedline` 0.53.0 |
| Archive source revision | `14be1e0f597b359426b5a5b5ee0b148c701fb38a` |
| Archive SHA-256 | `9b061d8059603bc95c569166a79ca60c01ccc8f8b6065035aed66a3753fdb06c` |
| Reconciled upstream source | `5004447cbc728b4de325e89aca298a5e528ec54d` |
| Brush fork imported in `a23d7ccc` | `BryanHeBY/reedline` at `53021f024ab219dd99e8d551e362436ba118455a` |

The archive checksum identifies the published `.crate` file. The upstream
base tracks the source reconciliation independently of that release. The
original MIT license is included in [LICENSE](LICENSE).

Brush publishes the vendor as `brush-reedline` 0.53.0 with library crate name
`reedline`. Publish `brush-reedline` before `brush-interactive` so crates.io
consumers resolve the fork.

## Sync to `5004447`

The official upstream Git data was acquired over HTTPS with server certificate
verification. The local `verified-main` ref resolved to `5004447`. The
archive revision is its ancestor, with 23 commits in the range. Comparing
upstream paths under `brush-reedline/` showed that the vendor already carried
the first 20 commits through `452867edff3660abad7dc54c8ed96d0f36373c7e`.
The last three were reconciled against that tree and the existing fork.

| Upstream commit | Coverage in Brush |
| --- | --- |
| `695cf54` | Already present. Simplified auto-pair condition in the engine and example. |
| `3f9c7c0` | Already present. Removed the Codecov badge. |
| `c87e337` | Already present. Unix raw mode keeps output processing, with a PTY test and the `rustix` dev features it needs. |
| `509e6d3` | Already present. Synchronized repaint avoids flicker during typing. |
| `748b0f0` | Already present. Description menu loads the first entry's examples. |
| `b83d720` | Already present. CWD hinter and keybinding query tests use Brush formatting. |
| `28bffa1` | Already present. Read-only history loading coexists with Brush navigation queries. |
| `1893856` | Already present. Plain repaint draws over the previous frame and erases the remainder. |
| `092bdb3` | Already present. List menu moves selection on the first press past a page. |
| `9424ef7` | Already present. Menu rendering, paging, and completer dispatch tests. |
| `fd043fd` | Already present. IDE menu avoids NUL padding and keeps its plain selection inside the box. |
| `119ce00` | Already present. Fallback highlights map to the original graphemes. |
| `536194e` | Already present. The engine paints a request from the last dropped repaint handle, with ordered input still in place. |
| `96ae188` | Already present. A soft-wrapped hint is clipped to the visible screen. |
| `d850931` | Already present. External output can release the editor's polling requirement. |
| [Wordmark update](https://github.com/nushell/reedline/commit/3ede01df2dda2e69480b4ea9402a7c570c78010d) | Already present. README, contributor guide, and wordmark with Brush file hygiene. |
| `58b32ba` | Already present. A completion that outlives its menu invalidates the prompt anchor. |
| `ef07d39` | Already present. Prompt footer, example, and tests coexist with Brush's status-line callback. |
| `f1d6069` | Already present in the manifest. `rusqlite` 0.43.0 and both SQLite feature paths are retained. |
| `452867e` | Already present. Completers can report taking the terminal, and Brush retains its cursor-query callback. |
| `8f8a7b3` | Reconciled. Count the row shared by an empty indicator and test repeated paints of a tall buffer. |
| `00e4875` | Reconciled. Keep CRLF as a line break, escape a lone CR, use LF between built-in menu and footer rows, and style continuation rows of examples individually. |
| `5004447` | Reconciled. The painter writer emits CRLF, including across writes and retries. History session IDs use the painter's stderr. Large-buffer clipping retains carriage return at the cut. |

The writer change keeps owned `PromptLines` pieces where Brush's horizontal
scrolling needs them and leaves the unclipped text available on exit. The
interrupt caret marker still prints before the final line break. Mouse hit
testing recognizes CRLF as one break and preserves original byte offsets.
Host menus can supply LF or CRLF, including a trailing line ending. Existing
screen, layout, and menu tests were kept. Assertions on internal menu strings
now use LF, while terminal-output assertions still check CRLF. The removed
`coerce_crlf` tests are covered by writer tests, including the leading
continuation regression.

The upstream repository lockfile remains upstream's lockfile. Brush keeps its
tracked workspace lock and resolves this independently packaged vendor through
its manifest. The workspace lockfile was unchanged by this vendor sync.
Both upstream revisions carry the same MIT license as the vendor.

## Formatter baselines

At both upstream revisions, `.github/workflows/ci.yml` uses floating stable
for `cargo fmt --all -- --check` on Ubuntu. There is no `rust-toolchain` or
rustfmt configuration at either revision. Upstream uses Rust edition 2021
with rustfmt's default style edition 2015. The manifest declares Rust 1.95.0,
which CI checks in a separate `1.95` job. Both
pristine source exports passed the format check locally with stable resolving
to Rust 1.99.0 (`b940084d7`) and rustfmt 1.10.0-stable (`b940084d7e`).

The final in-tree Rust edits use Brush's Rust 1.100.0 pin (`0ef1bc386`),
rustfmt 1.11.0-stable (`0ef1bc3860`), and edition and style edition 2024
from the workspace configuration. Brush retains the vendor's Rust 1.95.0
manifest minimum.

## Behavior to carry forward

- Ordered input and `EditMode::next_replay_event` keep macro playback ahead
  of queued physical input and across submissions and host callbacks. The
  optional input source and cursor-position query share the host's stream.
- Readline integration uses byte-oriented editing, editable history, and line
  display controls for horizontal scrolling, active regions, and showing
  edits to recalled entries. Preserve multiline caret movement, prefix search
  reset after editing, configured history deduplication, and restoration of
  the draft and its caret. The default arrow history search lands at the end
  of the recalled buffer. Explicit prefix-search bindings keep their prefix
  caret. See Brush commits `fe06303f`, `19ac84a5`, and `99e02506`.
- Preserve highlighting in the live buffer and on submission, including
  styling only the typed prefix when a history suggestion is shown.
- Preserve CRLF as a single line ending in editing, history recall, and
  repainting. Keep the PTY tests' native ioctl request on macOS and their
  cursor-query replies and repaint-aware output expectations.

Keep shell configuration and key bindings in `brush-interactive`. The build
guide documents the sync procedure and regression checks.
[Maintaining the Reedline vendor](../docs/how-to/build.md#maintaining-the-reedline-vendor)
