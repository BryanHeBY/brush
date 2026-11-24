/// Options for a shell user interface.
///
/// New optional settings are added to this struct as the shell gains features. Code that
/// constructs it with a complete struct literal must add every new field; prefer the builder
/// or a struct-update expression so that later additions keep compiling:
///
/// ```
/// let options = brush_interactive::UIOptions::builder()
///     .history_share(true)
///     .build();
/// assert!(options.history_share);
///
/// let options = brush_interactive::UIOptions {
///     history_share: true,
///     ..Default::default()
/// };
/// assert!(options.history_share);
/// ```
#[derive(Default, bon::Builder)]
pub struct UIOptions {
    /// Whether to disable bracketed paste mode.
    #[builder(default)]
    pub disable_bracketed_paste: bool,
    /// Whether to disable color.
    #[builder(default)]
    pub disable_color: bool,
    /// Whether to disable syntax highlighting.
    #[builder(default)]
    pub disable_highlighting: bool,
    /// Whether Up and Down browse visible history suggestions before inserting them.
    #[builder(default)]
    pub history_suggestion_navigation: bool,
    /// Whether to enable terminal integration.
    #[builder(default)]
    pub terminal_shell_integration: bool,
    /// Whether to enable zsh-style hooks.
    #[builder(default)]
    pub zsh_style_hooks: bool,
    /// Whether to append each command to the history file as soon as it finishes.
    #[builder(default)]
    pub history_append_immediately: bool,
    /// Whether to share the history file with other sessions.
    #[builder(default)]
    pub history_share: bool,
    /// How history file writes handle concurrency and replacement.
    #[builder(default)]
    pub history_write_mode: brush_core::history::HistoryWriteMode,
}

impl From<&UIOptions> for crate::InteractiveOptions {
    fn from(options: &UIOptions) -> Self {
        Self {
            terminal_shell_integration: options.terminal_shell_integration,
            zsh_style_hooks: options.zsh_style_hooks,
            history_append_immediately: options.history_append_immediately,
            history_share: options.history_share,
            history_write_mode: options.history_write_mode,
        }
    }
}
