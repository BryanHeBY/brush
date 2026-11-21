use crate::input_backend::InteractivePrompt;

impl reedline::Prompt for InteractivePrompt {
    fn render_prompt_left(&self) -> std::borrow::Cow<'_, str> {
        // [Workaround: see https://github.com/nushell/reedline/issues/707]
        // If the prompt starts with a newline character, then there's a chance
        // that it won't be rendered correctly. For this specific case, insert
        // an extra space character before the newline.
        if self.prompt.starts_with('\n') {
            std::format!(" {}", self.prompt).into()
        } else {
            self.prompt.as_str().into()
        }
    }

    fn render_prompt_right(&self) -> std::borrow::Cow<'_, str> {
        self.alt_side_prompt.as_str().into()
    }

    // N.B. For now, we don't support prompt indicators.
    fn render_prompt_indicator(
        &self,
        _prompt_mode: reedline::PromptEditMode,
    ) -> std::borrow::Cow<'_, str> {
        "".into()
    }

    fn render_prompt_multiline_indicator(&self) -> std::borrow::Cow<'_, str> {
        self.continuation_prompt.as_str().into()
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: reedline::PromptHistorySearch,
    ) -> std::borrow::Cow<'_, str> {
        match history_search.status {
            reedline::PromptHistorySearchStatus::Passing => {
                if history_search.term.is_empty() {
                    "(rev search) ".into()
                } else {
                    std::format!("(rev search: {}) ", history_search.term).into()
                }
            }
            reedline::PromptHistorySearchStatus::Failing => {
                std::format!("(failing rev search: {}) ", history_search.term).into()
            }
        }
    }

    fn get_prompt_color(&self) -> reedline::Color {
        reedline::Color::Default
    }

    fn get_prompt_multiline_color(&self) -> nu_ansi_term::Color {
        nu_ansi_term::Color::LightBlue
    }

    fn get_indicator_color(&self) -> reedline::Color {
        reedline::Color::Cyan
    }

    fn get_prompt_right_color(&self) -> reedline::Color {
        reedline::Color::Fixed(5)
    }

    fn right_prompt_on_last_line(&self) -> bool {
        false
    }
}

/// Adds the configured Readline mode indicator to the beginning of the prompt.
pub(super) struct ReadlinePrompt<'a> {
    pub prompt: &'a InteractivePrompt,
    pub options: &'a brush_core::ReadlineOptions,
    pub keymaps: &'a tokio::sync::Mutex<super::edit_mode::BindingMaps>,
}

fn mode_text(text: &str) -> String {
    use brush_parser::readline_binding::{KeySequenceItem, parse_key_sequence};

    let Ok(sequence) = parse_key_sequence(text) else {
        return text.to_owned();
    };
    let bytes: Vec<_> = sequence
        .0
        .into_iter()
        .filter_map(|item| match item {
            KeySequenceItem::Byte(byte) if !matches!(byte, 1 | 2) => Some(byte),
            _ => None,
        })
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

impl reedline::Prompt for ReadlinePrompt<'_> {
    fn render_prompt_left(&self) -> std::borrow::Cow<'_, str> {
        use reedline::{EditMode as _, PromptEditMode, PromptViMode};
        let prompt = self.prompt.render_prompt_left();
        if !self.options.show_mode_in_prompt {
            return prompt;
        }
        let mode = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(self.keymaps.lock())
                .edit_mode()
        });
        let indicator = match mode {
            PromptEditMode::Vi(PromptViMode::Insert) => &self.options.vi_ins_mode_string,
            PromptEditMode::Vi(_) => &self.options.vi_cmd_mode_string,
            _ => &self.options.emacs_mode_string,
        };
        format!("{}{prompt}", mode_text(indicator)).into()
    }

    fn render_prompt_right(&self) -> std::borrow::Cow<'_, str> {
        self.prompt.render_prompt_right()
    }

    fn render_prompt_indicator(&self, mode: reedline::PromptEditMode) -> std::borrow::Cow<'_, str> {
        self.prompt.render_prompt_indicator(mode)
    }

    fn render_prompt_multiline_indicator(&self) -> std::borrow::Cow<'_, str> {
        self.prompt.render_prompt_multiline_indicator()
    }

    fn render_prompt_history_search_indicator(
        &self,
        search: reedline::PromptHistorySearch,
    ) -> std::borrow::Cow<'_, str> {
        self.prompt.render_prompt_history_search_indicator(search)
    }

    fn get_prompt_color(&self) -> reedline::Color {
        self.prompt.get_prompt_color()
    }

    fn get_prompt_multiline_color(&self) -> nu_ansi_term::Color {
        self.prompt.get_prompt_multiline_color()
    }

    fn get_indicator_color(&self) -> reedline::Color {
        self.prompt.get_indicator_color()
    }

    fn get_prompt_right_color(&self) -> reedline::Color {
        self.prompt.get_prompt_right_color()
    }

    fn right_prompt_on_last_line(&self) -> bool {
        self.prompt.right_prompt_on_last_line()
    }
}
