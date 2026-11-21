use std::{collections::VecDeque, fs, path::Path};

use brush_core::completion::{Completions, ProcessingOptions};
use crossterm::event::KeyEvent;
use reedline::KeyCode;
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub(super) struct Listings {
    output: Vec<String>,
    visible: Vec<String>,
    pending: Option<PendingListing>,
}

struct PendingListing {
    lines: VecDeque<String>,
    confirm: bool,
    page_size: usize,
}

impl Listings {
    pub(super) fn cancel(&mut self) {
        self.pending = None;
        self.dismiss();
    }

    pub(super) fn dismiss(&mut self) -> bool {
        let was_visible = !self.visible.is_empty();
        self.visible.clear();
        was_visible
    }

    pub(super) fn status(&self) -> String {
        self.visible.join("\n")
    }

    pub(super) fn print_overflow(&mut self) {
        self.output.append(&mut self.visible);
    }

    pub(super) fn print_message(&mut self, message: String) {
        self.output.push(message);
    }

    pub(super) fn drain(&mut self) -> Vec<String> {
        std::mem::take(&mut self.output)
    }

    /// Queues a listing. Returns whether the user must confirm before it can be drawn.
    pub(super) fn show(
        &mut self,
        completions: &Completions,
        shell: &brush_core::Shell<impl brush_core::ShellExtensions>,
        use_color: bool,
        utf8_locale: bool,
    ) -> bool {
        let candidates = &completions.candidates;
        let processing = &completions.options;
        let working_dir = shell.working_dir();
        let options = shell.readline_options();
        let ls_colors = shell.env_str("LS_COLORS");
        let home = shell.env_str("HOME");
        let (columns, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let columns = usize::from(columns);
        let columns = options
            .completion_display_width
            .unwrap_or(columns)
            .min(columns);
        let page_size = if options.page_completions {
            usize::from(rows).saturating_sub(2).max(1)
        } else {
            usize::MAX
        };
        let display = candidates
            .iter()
            .map(|candidate| {
                DisplayName::new(
                    candidate,
                    processing,
                    working_dir,
                    home.as_deref(),
                    options.visible_stats,
                )
            })
            .collect::<Vec<_>>();
        let common_len = crate::completion::common_prefix_len(
            &display
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            options.completion_ignore_case,
        );
        let abbreviate = options.completion_prefix_display_length > 0
            && display
                .first()
                .and_then(|entry| entry.name.get(..common_len))
                .is_some_and(|prefix| prefix.width() > options.completion_prefix_display_length);
        let names = display
            .into_iter()
            .map(|entry| {
                let name = if abbreviate {
                    abbreviate_name(entry.name, common_len)
                } else {
                    entry.name
                };
                let output_meta = options.output_meta || utf8_locale;
                let mut rendered = visible_text(&name, output_meta);
                let cells = rendered.width() + usize::from(entry.marker.is_some());
                if options.colored_stats
                    && use_color
                    && let Some(kind) = entry.kind
                    && let Some(color) = file_color(ls_colors.as_deref(), kind)
                {
                    rendered = format!("\x1b[{color}m{rendered}\x1b[0m");
                }
                if !abbreviate
                    && options.colored_completion_prefix
                    && use_color
                    && let Some(prefix) = name.get(..common_len).filter(|prefix| !prefix.is_empty())
                    && let Some(suffix) = name.get(common_len..)
                {
                    rendered = format!(
                        "\x1b[01;31m{}\x1b[0m{}",
                        visible_text(prefix, output_meta),
                        visible_text(suffix, output_meta)
                    );
                }
                if let Some(marker) = entry.marker {
                    rendered.push(marker);
                }
                (rendered, cells)
            })
            .collect::<Vec<_>>();

        let confirm = options.completion_query_items != 0
            && candidates.len() >= options.completion_query_items;
        self.pending = Some(PendingListing {
            lines: format_columns(&names, columns, options.print_completions_horizontally),
            confirm,
            page_size,
        });
        if confirm {
            self.visible = vec![format!(
                "Display all {} possibilities? (y or n)",
                candidates.len()
            )];
        } else {
            self.next_page(page_size);
        }
        confirm
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> bool {
        let Some(pending) = &self.pending else {
            return false;
        };
        if key.modifiers.contains(reedline::KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
        {
            self.cancel();
            return false;
        }
        if key.code == KeyCode::Esc
            || key.modifiers.contains(reedline::KeyModifiers::CONTROL)
                && key.code == KeyCode::Char('g')
        {
            self.cancel();
            return true;
        }
        match (pending.confirm, key.code) {
            (true, KeyCode::Char('y' | 'Y')) => self.next_page(pending.page_size),
            (true, KeyCode::Char('n' | 'N')) | (false, KeyCode::Char('q' | 'Q')) => self.cancel(),
            (false, KeyCode::Char(' ')) => self.next_page(pending.page_size),
            (false, KeyCode::Enter) => self.next_page(1),
            _ => {}
        }
        true
    }

    fn next_page(&mut self, count: usize) {
        if let Some(pending) = &mut self.pending {
            pending.confirm = false;
            let count = count.min(pending.lines.len());
            self.visible.clear();
            self.visible.extend(pending.lines.drain(..count));
            if pending.lines.is_empty() {
                self.pending = None;
            } else {
                self.visible.push("--More--".into());
            }
        }
    }
}

struct DisplayName {
    name: String,
    marker: Option<char>,
    kind: Option<&'static str>,
}

impl DisplayName {
    fn new(
        candidate: &str,
        processing: &ProcessingOptions,
        working_dir: &Path,
        home: Option<&str>,
        visible_stats: bool,
    ) -> Self {
        let word = strip_completion_space(candidate);
        if !processing.treat_as_filenames {
            return Self {
                name: word.to_owned(),
                marker: None,
                kind: None,
            };
        }
        let path = if processing.no_autoquote_filenames {
            word.to_owned()
        } else {
            brush_parser::unquote_str(word)
        };
        let path = brush_core::sys::fs::strip_path_separator_suffix(&path);
        let name = Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(path);
        let absolute = match (path.strip_prefix("~/"), home) {
            (Some(relative), Some(home)) => Path::new(home).join(relative),
            _ => working_dir.join(path),
        };
        let (kind, marker) =
            fs::symlink_metadata(absolute).map_or(("mi", None), |metadata| file_kind(&metadata));
        let marker = if visible_stats || marker == Some('/') {
            marker
        } else {
            None
        };
        Self {
            name: name.to_owned(),
            marker,
            kind: Some(kind),
        }
    }
}

fn abbreviate_name(name: String, common_len: usize) -> String {
    let Some(suffix) = name.get(common_len..).filter(|suffix| !suffix.is_empty()) else {
        return name;
    };
    let ellipsis = if suffix.starts_with('.') {
        "___"
    } else {
        "..."
    };
    format!("{ellipsis}{suffix}")
}

fn strip_completion_space(text: &str) -> &str {
    let Some(prefix) = text.strip_suffix(' ') else {
        return text;
    };
    let escapes = prefix.len() - prefix.trim_end_matches('\\').len();
    if escapes % 2 == 0 { prefix } else { text }
}

fn file_kind(metadata: &fs::Metadata) -> (&'static str, Option<char>) {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return ("ln", Some('@'));
    }
    if file_type.is_dir() {
        return ("di", Some('/'));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};
        if file_type.is_fifo() {
            return ("pi", Some('|'));
        }
        if file_type.is_socket() {
            return ("so", Some('='));
        }
        if file_type.is_block_device() {
            return ("bd", None);
        }
        if file_type.is_char_device() {
            return ("cd", None);
        }
        if metadata.permissions().mode() & 0o111 != 0 {
            return ("ex", Some('*'));
        }
    }
    ("fi", None)
}

fn file_color<'a>(ls_colors: Option<&'a str>, kind: &str) -> Option<&'a str> {
    if let Some(color) = ls_colors.and_then(|colors| {
        colors
            .rsplit(':')
            .filter_map(|entry| entry.split_once('='))
            .find_map(|(name, color)| (name == kind).then_some(color))
    }) {
        return color
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b';')
            .then_some(color)
            .filter(|color| !color.is_empty());
    }
    match kind {
        "di" => Some("01;34"),
        "ln" => Some("01;36"),
        "ex" => Some("01;32"),
        "pi" => Some("33"),
        "so" => Some("01;35"),
        "bd" | "cd" => Some("01;33"),
        _ => None,
    }
}

fn visible_text(text: &str, output_meta: bool) -> String {
    use std::fmt::Write as _;
    let mut visible = String::new();
    for character in text.chars() {
        match character {
            '\x00'..='\x1f' => {
                visible.push('^');
                visible.push(char::from((character as u8) + b'@'));
            }
            '\x7f' => visible.push_str("^?"),
            _ if character.is_control() || !output_meta && !character.is_ascii() => {
                for byte in character.encode_utf8(&mut [0; 4]).bytes() {
                    let _ = write!(visible, "\\{byte:03o}");
                }
            }
            _ => visible.push(character),
        }
    }
    visible
}

fn format_columns(
    names: &[(String, usize)],
    columns: usize,
    horizontally: bool,
) -> VecDeque<String> {
    let width = names.iter().map(|(_, cells)| *cells).max().unwrap_or(0) + 2;
    let per_row = (columns / width).max(1);
    let rows = names.len().div_ceil(per_row);
    (0..rows)
        .map(|row| {
            let mut line = String::new();
            let (start, stop, step) = if horizontally {
                (row * per_row, per_row, 1)
            } else {
                (row, per_row, rows)
            };
            let mut entries = names.iter().skip(start).step_by(step).take(stop).peekable();
            while let Some((name, cells)) = entries.next() {
                line.push_str(name);
                if entries.peek().is_some() {
                    line.push_str(&" ".repeat(width - cells));
                }
            }
            line
        })
        .collect()
}
