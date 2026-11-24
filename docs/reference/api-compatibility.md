# API compatibility notes

These notes cover source-compatibility changes in brush's published crates. They are
additive unless marked otherwise.

## `UIOptions` gained history settings

`brush-interactive::UIOptions` gained three fields:

- `history_suggestion_navigation` (default `false`)
- `history_append_immediately` (default `false`)
- `history_share` (default `false`)

The builder and `Default` implementations continue to work. Downstream code that initializes
`UIOptions` with an **exhaustive struct literal** must add the new fields (or switch to a
struct-update expression). A complete literal that used to compile now fails with
missing-field errors; this is a source-compatibility break for that initialization style only.

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

`brush_shell::config::UiConfig` gained `history_suggestion_navigation` (an
`Option<bool>`; absent means `false`). The append/share settings are **not** in `UiConfig`:
they are the `append_immediately` and `share` fields of
`brush_shell::config::Config::history` (`HistoryConfig`).

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

Configuration files set these under `[ui] history-suggestion-navigation` and
`[history] append-immediately` / `share`; see the [configuration
reference](configuration.md) and the generated `schemas/config.schema.json`.
