# BryanHeBY's Personal Shell Configuration

These optional examples capture my Bash and Brush setup on Omarchy for
the BryanHeBY Brush fork.

## Starship prompt

The [ys-inspired theme](starship-ys.toml) shows the username, host,
directory, Git status, and time on the first line, with the input on
the second. Failed commands also show their exit status.

With [Starship](https://starship.rs/guide/) installed, copy the theme
from the repository root.

```bash
starship_config_dir="${XDG_CONFIG_HOME:-$HOME/.config}"
mkdir -p "$starship_config_dir"
cp -i samples-fork/starship-ys.toml "$starship_config_dir/starship-ys.toml"
```

Add this to the interactive section of `~/.bashrc` for Bash or Brush.

```bash
export STARSHIP_CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/starship-ys.toml"
if command -v starship >/dev/null 2>&1; then
    eval "$(starship init bash)"
fi
```

If your `~/.bashrc` already initializes Starship, add just the export
before that initialization. On Omarchy, put the export before the line
that sources the Omarchy Bash configuration, which initializes Starship.

## Brush input and Herdr

The [Brush configuration](config.toml) enables input syntax highlighting,
keeps the default history navigation, and enables directory reporting
in Herdr through terminal integration.

To try it for one session, run this from the repository root.

```bash
brush --config samples-fork/config.toml
```

For persistent use on Linux or macOS, merge the settings into
`${XDG_CONFIG_HOME:-$HOME/.config}/brush/config.toml`.
