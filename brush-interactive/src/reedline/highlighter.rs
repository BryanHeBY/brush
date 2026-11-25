use std::borrow::Cow;

use crate::styles::kind_to_style;
use crate::{highlighting, refs};
use nu_ansi_term::Style;

pub(crate) struct ReedlineHighlighter<SE: brush_core::ShellExtensions> {
    pub shell: refs::ShellRef<SE>,
    pub italics: bool,
}

pub(crate) struct PlainTextHighlighter;

impl reedline::Highlighter for PlainTextHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> reedline::StyledText {
        let mut styled = reedline::StyledText::new();
        styled.push((Style::new(), line.to_owned()));
        styled
    }
}

impl<SE: brush_core::ShellExtensions> reedline::Highlighter for ReedlineHighlighter<SE> {
    fn highlight(&self, line: &str, cursor: usize) -> reedline::StyledText {
        self.highlight_with_hint(line, cursor, None)
    }

    fn highlight_with_hint(
        &self,
        line: &str,
        cursor: usize,
        hint: Option<&str>,
    ) -> reedline::StyledText {
        let context = hint.map_or(Cow::Borrowed(line), |hint| {
            Cow::Owned(line.to_owned() + hint)
        });
        let highlighted = {
            let shell = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(self.shell.lock())
            });
            highlighting::highlight_command(shell.as_ref(), &context, cursor)
        };

        let mut styled = reedline::StyledText::new();
        for span in highlighted
            .spans()
            .iter()
            .take_while(|span| span.range.start < line.len())
        {
            if let Some(text) = line.get(span.range.start..span.range.end.min(line.len())) {
                styled.push((kind_to_style(span.kind, self.italics), text.to_owned()));
            }
        }

        styled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reedline::Highlighter;

    #[test]
    fn plain_text_highlighter_uses_terminal_default_colors() {
        let styled = PlainTextHighlighter.highlight("echo hello", 0);

        assert_eq!(styled.buffer.len(), 1);
        assert_eq!(styled.buffer[0].0.foreground, None);
        assert_eq!(styled.buffer[0].0.background, None);
        assert_eq!(styled.buffer[0].1, "echo hello");
    }
}
