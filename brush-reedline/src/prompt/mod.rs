mod base;
mod default;

pub use base::PromptHelixMode;
pub use base::{
    DEFAULT_INDICATOR_COLOR, DEFAULT_PROMPT_COLOR, DEFAULT_PROMPT_MULTILINE_COLOR,
    DEFAULT_PROMPT_RIGHT_COLOR, Prompt, PromptEditMode, PromptEditModeDiscriminants,
    PromptHistorySearch, PromptHistorySearchStatus, PromptViMode,
};

pub use default::{
    DEFAULT_INSERT_PROMPT_INDICATOR, DEFAULT_MULTILINE_INDICATOR, DEFAULT_NORMAL_PROMPT_INDICATOR,
    DEFAULT_PROMPT_INDICATOR, DEFAULT_SELECT_PROMPT_INDICATOR, DefaultPrompt, DefaultPromptSegment,
};
