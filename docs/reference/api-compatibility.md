# API compatibility notes

These notes cover source-compatibility changes in brush's published crates. Adding a
public struct field breaks exhaustive struct literals and patterns, even when the field
has a default.

## `UIOptions` gained UI and history settings

`brush-interactive::UIOptions` gained six fields:

- `italics` (default `false`)
- `history_suggestion_navigation` (default `false`)
- `history_navigation_dedup` (default `HistoryNavigationDedup::Unique`)
- `history_append_immediately` (default `false`)
- `history_share` (default `false`)
- `history_write_mode` (default `HistoryWriteMode::Portable`)

The struct currently has 11 public fields in total. Both the builder and `Default`
use the defaults above.

The builder and `Default` implementations continue to work. Downstream code that initializes
`UIOptions` with an **exhaustive struct literal** must add the new fields (or switch to a
struct-update expression). A complete literal that used to compile now fails with
missing-field errors; exhaustive patterns must also account for the fields.

Preferred initialization:

```rust
// Builder
let options = brush_interactive::UIOptions::builder()
    .history_suggestion_navigation(true)
    .history_append_immediately(true)
    .history_share(true)
    .build();

// Default plus overrides
let options = brush_interactive::UIOptions {
    history_suggestion_navigation: true,
    history_append_immediately: true,
    history_share: true,
    ..Default::default()
};
```

## `UiConfig` and `HistoryConfig` in `brush-shell`

`brush_shell::config::UiConfig` gained `italics` and `history_suggestion_navigation`
(`Option<bool>`) and `history_navigation_dedup` (`Option<HistoryNavigationDedupConfig>`).
`UiConfig::default()` leaves these fields as `None`; when applied to UI options, absent
values mean `false`, `false`, and `HistoryNavigationDedup::Unique`, respectively.
`UiConfig` currently has four public fields in total, including `syntax_highlighting`.

The append, share, and write-mode settings are the `append_immediately`, `share`, and
`write_mode` fields of `brush_shell::config::Config::history` (`HistoryConfig`), not
`UiConfig` fields. `HistoryConfig::default()` leaves them as `None`; absent values are
applied as `false`, `false`, and `HistoryWriteMode::Portable`, respectively.

Exhaustive `UiConfig` or `HistoryConfig` literals likewise need the new fields or a
struct-update expression:

```rust
let ui = brush_shell::config::UiConfig {
    history_suggestion_navigation: Some(true),
    ..Default::default()
};

let history = brush_shell::config::HistoryConfig {
    append_immediately: Some(true),
    share: Some(true),
    ..Default::default()
};
```

Configuration files set these under `[ui] italics`, `history-suggestion-navigation`,
and `history-navigation-dedup`, and `[history] append-immediately`, `share`, and
`write-mode`; see the [configuration reference](configuration.md) and the generated
`schemas/config.schema.json`.
