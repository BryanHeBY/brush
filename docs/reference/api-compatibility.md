# API compatibility notes

These notes cover source-compatibility changes in brush's published crates. They are
additive unless marked otherwise.

## `UIOptions` and `UiConfig` gained history settings

`brush-interactive::UIOptions` gained `history_append_immediately` and `history_share`;
`brush_shell::config::UiConfig` gained the matching configuration fields at the same time.
The builder and `Default` implementations continue to work.

Downstream code that initializes either struct with an **exhaustive struct literal** must add
the new fields (or switch to a struct-update expression). A complete literal that used to
compile now fails with missing-field errors; this is a source-compatibility break for that
initialization style only.

Preferred initialization:

```rust
// Builder
let options = brush_interactive::UIOptions::builder()
    .history_append_immediately(true)
    .history_share(true)
    .build();

// Default plus overrides
let options = brush_interactive::UIOptions {
    history_append_immediately: true,
    history_share: true,
    ..Default::default()
};

let config = brush_shell::config::UiConfig {
    history_suggestion_navigation: Some(true),
    ..Default::default()
};
```

`UIOptions` defaults both new fields to `false`. `UiConfig` fields are optional and absent
means `false`; see the [configuration reference](configuration.md) and the generated
`schemas/config.schema.json`.
