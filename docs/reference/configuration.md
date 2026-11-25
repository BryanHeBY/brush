# Configuration File

brush supports an optional TOML configuration file that allows you to customize shell behavior without command-line arguments.

## File Location

`brush` looks for the configuration file at:

- **Linux/macOS**: `${XDG_CONFIG_HOME}/brush/config.toml`*
- **Windows**: `%APPDATA%\brush\config.toml`

> [!NOTE]
> On Linux/macOS falls back to `~/.config/brush/config.toml` if `XDG_CONFIG_HOME` is undefined.

You can override this location with the `--config` flag:

```bash
brush --config /path/to/custom/config.toml
```

To disable configuration file loading entirely, use:

```bash
brush --no-config
```

## Configuration Priority

Settings are applied in the following order (later values override earlier ones):

1. **Defaults** - Built-in default values
2. **Configuration file** - Values from `config.toml`
3. **Command-line arguments** - Flags passed to brush

## File Format

The configuration file uses [TOML](https://toml.io/) format. All settings are optional; brush uses sensible defaults for any unspecified values.

### Example Configuration

```toml
[ui]
syntax-highlighting = true

[history]
append-immediately = true

[experimental]
zsh-hooks = true
terminal-shell-integration = true
```

## Available Settings

### `[ui]` Section

User interface settings.

| Setting               | Type    | Default | CLI flag                | Description                                  |
|-----------------------|---------|---------|-------------------------|----------------------------------------------|
| `syntax-highlighting` | boolean | `true` | `--enable-highlighting` | Enable syntax highlighting in the input line |
| `italics` | boolean | `false` | | Allow italics in syntax highlighting and Reedline history suggestions |
| `history-suggestion-navigation` | boolean | `false` | | Browse visible history suggestions with Up and Down and accept with Right in the `reedline` backend |
| `history-navigation-dedup` | `"unique"`, `"adjacent"`, or `"bash"` | `"unique"` | | Duplicate handling for Up/Down history traversal and explicit prefix searches in Reedline |

Set `syntax-highlighting = false` to turn highlighting off. The
`--enable-highlighting` flag overrides that setting for the current invocation.

Syntax highlighting is supported by the `reedline` and `vt` input backends.
The `--disable-color` flag also disables syntax highlighting.
Options such as `-l` and `--color` are orange (`#FFB178`). Set `italics = true` to
also use italics for options, shell keywords, operators, and Reedline history
suggestions. This setting does not alter your prompt or Tab completion menu.

`history-suggestion-navigation` lets Up and Down preview older or newer
history matches at the end of an input line. The entire draft is the prefix,
including spaces, and stays editable even when there is no match. An empty
line uses the configured history bindings. Right accepts the displayed
suggestion. In Emacs editing mode, Ctrl+Right accepts up to the start of the
next word, including any separators crossed, or moves by word when there is
no suggestion.

History navigation with `"unique"` visits only the newest occurrence of each
exact command line in the current history, in either direction. `"adjacent"`
skips consecutive identical matches in the active search, but allows
duplicates separated by a different match. `"bash"` retains raw ordinary
navigation and skips consecutive duplicates in explicit prefix searches.
Case and whitespace remain significant for duplicate handling, even when a
prefix search is case-insensitive. This does not affect Ctrl+R, suggestions,
or commands saved to the history file.

### `[history]` Section

Command history settings.

| Setting               | Type    | Default | Description                                              |
|-----------------------|---------|---------|----------------------------------------------------------|
| `append-immediately`  | boolean | `false` | Append each command to the history file as it finishes   |
| `share`               | boolean | `false` | Import other sessions' new entries and append this session's |
| `write-mode`          | string  | `"portable"` | `"portable"` locks and atomically replaces; `"bash"` uses unlocked in-place writes |

By default, an interactive shell appends the commands it recorded when the session ends
normally. This is a Brush choice: Bash defaults to overwriting the file unless `histappend`
is enabled. If the terminal is closed, brush saves the history before exiting; commands still
running at that moment may not have been appended yet.

With `append-immediately = true`, brush appends the command right after it finishes instead
of waiting for the session to end. zsh's `INC_APPEND_HISTORY` and `SHARE_HISTORY` write while
the command is running; Brush's timing is closer to `INC_APPEND_HISTORY_TIME` and is
intentionally kept.

`write-mode` controls only file writes, not the history list's semantics:

- `"portable"` (default) serializes writers with a sidecar lock next to the history file and
  rewrites through a private temporary file plus an atomic rename. It preserves the target's
  permission bits and group on Unix, the discretionary ACL and its inheritance protection on
  Windows, and follows an existing history symlink. The directory must be writable.
- `"bash"` writes in place without a lock or temporary copy: append opens the file for append,
  and an overwrite truncates it in place. This is the closest match to Bash's documented file
  behavior and accepts the same concurrent-write races. It also works when the history
  directory is not writable but the file itself is.

Both modes create a new history file with mode 0600 on Unix; on Windows a new file inherits
the containing directory's access rules. Both write modes keep an existing file's access rules
as described above.

The lock is a Brush extension; Bash documents no cross-process history lock. Different `TMPDIR`
values do not affect the lock because it lives next to the resolved history target.

`share = true` is the zsh `SHARE_HISTORY` style: before each prompt brush imports entries
other sessions appended to the file, and after each command it appends this session's new
entries. It implies immediate appending. Only entries added since the last read are imported,
so repeated identical commands are preserved; normal prompts read only the newly appended
tail of the history file.

The history file is `$HISTFILE`, which defaults to `~/.brush_history`. Setting `HISTFILE` to
an empty string keeps history in memory but disables reading and writing the file.

Brush appends to the file by default, so several sessions can share it. `shopt -u histappend`
selects Bash's overwrite-on-exit behavior instead. `HISTSIZE` caps the in-memory history and
`HISTFILESIZE` caps the file. An empty, non-numeric, or negative value explicitly disables
that limit. If `HISTFILESIZE` is unset it follows `HISTSIZE`; if both are unset Brush keeps
history unbounded rather than inventing Bash's default 500. `HISTIGNORE` patterns (including
`&` for the previous command) are not recorded in interactive shells, and `HISTCONTROL`
still applies.

### `[experimental]` Section

Experimental features that may change or be removed in future versions.
Each setting has an equivalent command-line flag; CLI flags take
precedence over the configuration file. See the
[experimental features reference](experimental.md) for details on each
feature.

| Setting                      | Type    | Default | CLI flag                         | Description                           |
|------------------------------|---------|---------|----------------------------------|---------------------------------------|
| `zsh-hooks`                  | boolean | `false` | `--enable-zsh-hooks`             | Enable zsh-style preexec/precmd hooks |
| `terminal-shell-integration` | boolean | `false` | `--enable-terminal-integration`  | Enable terminal shell integration     |

## JSON Schema

A JSON Schema for the configuration file is available at [`schemas/config.schema.json`](../../schemas/config.schema.json). This can be used with editors that support schema-based validation and autocompletion for TOML files.

### Using the Schema with VS Code

To enable schema validation in VS Code with the [Even Better TOML](https://marketplace.visualstudio.com/items?itemName=tamasfe.even-better-toml) extension, add this to your `config.toml`:

```toml
#:schema https://raw.githubusercontent.com/reubeno/brush/main/schemas/config.schema.json

[ui]
syntax-highlighting = true
```

The `#:schema` directive tells the editor where to find the schema for validation and autocompletion.

### Using the Schema with Other Editors

Many editors support JSON Schema for TOML files. Consult your editor's documentation for how to associate a schema with a file. You can reference the schema via:

- **URL**: `https://raw.githubusercontent.com/reubeno/brush/main/schemas/config.schema.json`
- **Local path**: Point to `schemas/config.schema.json` in your brush source checkout

## Sample Configuration

A sample configuration file is available at [`samples/config.toml`](../../samples/config.toml) in the brush repository. You can copy this file to get started:

```bash
# Linux/macOS
mkdir -p ~/.config/brush
cp samples/config.toml ~/.config/brush/config.toml
```

## Forward Compatibility

brush ignores unknown settings in the configuration file. This allows configuration files to be shared across different versions of brush without causing errors.

## Error Handling

If the configuration file cannot be read or parsed, brush logs an error message and continues with default settings. The shell will still start normally.
