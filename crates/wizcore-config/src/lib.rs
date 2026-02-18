use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub hotkey: String,
    pub launch_at_startup: bool,
    pub theme: Theme,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    TerminalDark,
    TerminalLight,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hotkey: "`".to_string(),
            launch_at_startup: false,
            theme: Theme::TerminalDark,
        }
    }
}
