use std::path::{Path, PathBuf};

use brush_core::escape;

#[allow(dead_code)]
pub(crate) async fn complete_async(
    shell: &mut brush_core::Shell<impl brush_core::ShellExtensions>,
    line: &str,
    pos: usize,
) -> brush_core::completion::Completions {
    let working_dir = shell.working_dir().to_path_buf();
    let home = if shell.readline_options().expand_tilde {
        None
    } else {
        shell
            .env_str("HOME")
            .filter(|path| !path.is_empty())
            .map(|path| PathBuf::from(path.into_owned()))
    };
    let mut filenames = FilePresentation {
        working_dir: &working_dir,
        mark_directories: shell.completion_config().fallback_options.mark_directories,
        mark_symlinked_directories: shell.readline_options().mark_symlinked_directories,
        replace_home_with_tilde: None,
    };
    let skip_completed_text = shell.readline_options().skip_completed_text;

    // Intentionally ignore any errors that arise.
    let completion_future = shell.complete(line, pos);
    tokio::pin!(completion_future);

    // Wait for the completions to come back or interruption, whichever happens first.
    let result = tokio::select! {
        result = &mut completion_future => {
            result
        }
        _ = brush_core::sys::signal::await_ctrl_c() => {
            Err(brush_core::ErrorKind::Interrupted.into())
        },
    };

    let mut completions = result.unwrap_or_else(|_| brush_core::completion::Completions {
        insertion_index: pos,
        delete_count: 0,
        candidates: Vec::new(),
        options: brush_core::completion::ProcessingOptions::default(),
    });

    // Look at the line up to 'pos' to check if we're in an unterminated
    // single or double quote string.
    let mut quote_char: Option<char> = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if i >= pos {
            break;
        }

        if escaped {
            escaped = false;
            continue;
        }

        if let Some(q) = quote_char {
            if c == q {
                quote_char = None;
            }
        } else if c == '\\' {
            escaped = true;
        } else if c == '\'' || c == '\"' {
            quote_char = Some(c);
        }
    }

    let completing_end_of_line = pos == line.len();
    if line
        .get(completions.insertion_index..pos)
        .is_some_and(|text| text.starts_with("~/"))
    {
        filenames.replace_home_with_tilde = home.as_deref();
    }

    // Deduplicate the candidates (retaining order), then postprocess them.
    completions.candidates = completions
        .candidates
        .into_iter()
        .collect::<indexmap::IndexSet<_>>()
        .into_iter()
        .map(|candidate| {
            postprocess_completion_candidate(
                candidate,
                &completions.options,
                &filenames,
                completing_end_of_line,
                quote_char,
            )
        })
        .collect();

    if skip_completed_text
        && let [candidate] = completions.candidates.as_mut_slice()
        && let Some(after_cursor) = line.get(pos..).filter(|text| !text.is_empty())
        && let Some(word) = line.get(completions.insertion_index..pos)
        && let Some(suffix) = candidate.strip_prefix(word)
    {
        let matching = matching_prefix_len(suffix, after_cursor);
        completions.delete_count += matching;
        if matching == after_cursor.len() {
            append_completion_space(candidate, &completions.options);
        }
    }

    completions
}

/// Returns the byte length of the prefix these strings share.
pub(crate) fn matching_prefix_len(first: &str, second: &str) -> usize {
    first
        .chars()
        .zip(second.chars())
        .take_while(|(first, second)| first == second)
        .map(|(c, _)| c.len_utf8())
        .sum()
}

#[cfg(reedline_backend)]
pub(crate) fn common_prefix_len(candidates: &[impl AsRef<str>], ignore_case: bool) -> usize {
    let Some((first, rest)) = candidates.split_first() else {
        return 0;
    };
    let first = first.as_ref();
    rest.iter().fold(first.len(), |length, candidate| {
        first
            .get(..length)
            .unwrap_or_default()
            .chars()
            .zip(candidate.as_ref().chars())
            .take_while(|(left, right)| {
                left == right || ignore_case && left.eq_ignore_ascii_case(right)
            })
            .map(|(c, _)| c.len_utf8())
            .sum()
    })
}

#[allow(dead_code)]
struct FilePresentation<'a> {
    working_dir: &'a Path,
    mark_directories: bool,
    mark_symlinked_directories: Option<bool>,
    replace_home_with_tilde: Option<&'a Path>,
}

#[allow(dead_code)]
fn postprocess_completion_candidate(
    mut candidate: String,
    options: &brush_core::completion::ProcessingOptions,
    filenames: &FilePresentation<'_>,
    completing_end_of_line: bool,
    quote_char: Option<char>,
) -> String {
    if options.treat_as_filenames {
        // Check if it's a directory.
        if filenames.mark_directories && !brush_core::sys::fs::ends_with_path_separator(&candidate)
        {
            let candidate_path = Path::new(&candidate);
            let abs_candidate_path = if candidate_path.is_absolute() {
                PathBuf::from(candidate_path)
            } else {
                filenames.working_dir.join(candidate_path)
            };

            let mark = filenames.mark_symlinked_directories != Some(false)
                || !abs_candidate_path.is_symlink();
            if mark && abs_candidate_path.is_dir() {
                // Use forward slash: backslash is the shell escape character.
                candidate.push('/');
            }
        }

        if let Some(home) = filenames.replace_home_with_tilde
            && let Ok(relative) = Path::new(&candidate).strip_prefix(home)
        {
            let had_separator = brush_core::sys::fs::ends_with_path_separator(&candidate);
            candidate = format!("~/{}", relative.to_string_lossy());
            if had_separator && !candidate.ends_with('/') {
                candidate.push('/');
            }
        }

        if !options.no_autoquote_filenames {
            let quote_mode = match quote_char {
                Some('\'') => escape::QuoteMode::SingleQuote,
                Some('\"') => escape::QuoteMode::DoubleQuote,
                _ => escape::QuoteMode::BackslashEscape,
            };

            // Like bash, leave a `~` unquoted, so a `~user` completion still expands.
            let options = escape::QuoteOptions::builder()
                .preferred_mode(quote_mode)
                .leave_tilde(true)
                .build();
            candidate = escape::quote(&candidate, &options).to_string();
        }
    }
    if completing_end_of_line {
        append_completion_space(&mut candidate, options);
    }

    candidate
}

fn append_completion_space(
    candidate: &mut String,
    options: &brush_core::completion::ProcessingOptions,
) {
    if !options.no_trailing_space_at_end_of_line
        && (!options.treat_as_filenames
            || !brush_core::sys::fs::ends_with_path_separator(candidate))
    {
        candidate.push(' ');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Quotes a file-name candidate completed at the end of the line.
    fn quoted_file_name(candidate: &str) -> String {
        let options = brush_core::completion::ProcessingOptions {
            treat_as_filenames: true,
            ..Default::default()
        };
        postprocess_completion_candidate(
            candidate.to_owned(),
            &options,
            &FilePresentation {
                working_dir: Path::new("/nonexistent"),
                mark_directories: true,
                mark_symlinked_directories: None,
                replace_home_with_tilde: None,
            },
            true,
            None,
        )
    }

    #[test]
    fn file_names_are_quoted_like_bash() {
        // Like bash, a `~user` candidate isn't quoted, so it still expands...
        assert_eq!(quoted_file_name("~root"), "~root ");
        // ...but a leading `#` is, so it doesn't start a comment.
        assert_eq!(quoted_file_name("#hash"), r"\#hash ");
        assert_eq!(quoted_file_name("a b"), r"a\ b ");
        // Like bash, a comma isn't special on its own.
        assert_eq!(quoted_file_name("a,b"), "a,b ");
    }
}
