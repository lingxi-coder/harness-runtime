//! Explicit palettes shared by terminal and native transcript renderers.
use crate::render::{NamedColor, StyleColor};

/// One of claude-code's 6 renderable themes (`utils/theme.ts` `THEME_NAMES`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeName {
    /// Dark theme (`darkTheme`).
    Dark,
    /// Light theme (`lightTheme`).
    Light,
    /// Light colorblind-friendly theme (`lightDaltonizedTheme`).
    LightDaltonized,
    /// Dark colorblind-friendly theme (`darkDaltonizedTheme`).
    DarkDaltonized,
    /// Light ANSI-only theme (`lightAnsiTheme`).
    LightAnsi,
    /// Dark ANSI-only theme (`darkAnsiTheme`).
    DarkAnsi,
}

impl Default for ThemeName {
    /// Headless default: dark (matches `ThemeSetting::Auto.resolve()`).
    fn default() -> Self {
        ThemeName::Dark
    }
}

impl ThemeName {
    /// All renderable theme names, in `THEME_NAMES` order.
    pub const ALL: [ThemeName; 6] = [
        ThemeName::Dark,
        ThemeName::Light,
        ThemeName::LightDaltonized,
        ThemeName::DarkDaltonized,
        ThemeName::LightAnsi,
        ThemeName::DarkAnsi,
    ];

    /// Wire string, byte-for-byte with claude-code's `ThemeName`.
    #[must_use]
    pub const fn as_wire(self) -> &'static str {
        match self {
            ThemeName::Dark => "dark",
            ThemeName::Light => "light",
            ThemeName::LightDaltonized => "light-daltonized",
            ThemeName::DarkDaltonized => "dark-daltonized",
            ThemeName::LightAnsi => "light-ansi",
            ThemeName::DarkAnsi => "dark-ansi",
        }
    }

    /// Parse a wire string back to a `ThemeName`. `None` for unknown values.
    #[must_use]
    pub fn from_wire(s: &str) -> Option<ThemeName> {
        ThemeName::ALL.into_iter().find(|n| n.as_wire() == s)
    }
}

/// Map a claude-code `ansi:<name>` color string to a neutral `StyleColor`.
/// "Bright" → the `Bright*` named variant; non-bright → the plain named
/// variant. Used only by the two `-ansi` themes. (The `tui`-side
/// `StyleColorIocraftExt` maps these to the same crossterm colors M6 used.)
#[must_use]
pub fn ansi(name: &str) -> StyleColor {
    match name {
        "ansi:black" => StyleColor::Named(NamedColor::Black),
        "ansi:blackBright" => StyleColor::Named(NamedColor::BrightBlack),
        "ansi:red" => StyleColor::Named(NamedColor::Red),
        "ansi:redBright" => StyleColor::Named(NamedColor::BrightRed),
        "ansi:green" => StyleColor::Named(NamedColor::Green),
        "ansi:greenBright" => StyleColor::Named(NamedColor::BrightGreen),
        "ansi:yellow" => StyleColor::Named(NamedColor::Yellow),
        "ansi:yellowBright" => StyleColor::Named(NamedColor::BrightYellow),
        "ansi:blue" => StyleColor::Named(NamedColor::Blue),
        "ansi:blueBright" => StyleColor::Named(NamedColor::BrightBlue),
        "ansi:magenta" => StyleColor::Named(NamedColor::Magenta),
        "ansi:magentaBright" => StyleColor::Named(NamedColor::BrightMagenta),
        "ansi:cyan" => StyleColor::Named(NamedColor::Cyan),
        "ansi:cyanBright" => StyleColor::Named(NamedColor::BrightCyan),
        "ansi:white" => StyleColor::Named(NamedColor::White),
        "ansi:whiteBright" => StyleColor::Named(NamedColor::BrightWhite),
        _ => StyleColor::Default,
    }
}

/// `StyleColor::Rgb` shorthand.
const fn rgb(r: u8, g: u8, b: u8) -> StyleColor {
    StyleColor::Rgb(r, g, b)
}

/// The active render palette. One neutral `StyleColor` per claude-code `Theme`
/// key the lingxi TUI consumes. All RGB/ANSI values copied from
/// `claude-code/src/utils/theme.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// Default body text (`text`).
    pub text: StyleColor,
    /// Dim/inactive (`inactive`) — system hints, footnotes.
    pub dim: StyleColor,
    /// Error (`error`).
    pub error: StyleColor,
    /// Success / approved (`success`).
    pub success: StyleColor,
    /// Warning (`warning`).
    pub warning: StyleColor,
    /// Permission accent (`permission`) — picker header, dialogs.
    pub permission: StyleColor,
    /// Plan-mode accent (`planMode`).
    pub plan_mode: StyleColor,
    /// Suggestion / completion accent (`suggestion`).
    pub suggestion: StyleColor,
    /// Claude/assistant accent (`claude`) — assistant body color.
    pub claude: StyleColor,
    /// Diff added line bg/fg (`diffAdded`).
    pub diff_added: StyleColor,
    /// Diff removed line (`diffRemoved`).
    pub diff_removed: StyleColor,
    /// Diff added word-level (`diffAddedWord`).
    pub diff_added_word: StyleColor,
    /// Diff removed word-level (`diffRemovedWord`).
    pub diff_removed_word: StyleColor,
}

impl Default for Theme {
    /// Headless default palette: dark (matches `AppState`'s default).
    fn default() -> Self {
        Theme::dark()
    }
}

impl Theme {
    /// Dark theme — `theme.ts` `darkTheme`.
    #[must_use]
    pub const fn dark() -> Theme {
        Theme {
            text: rgb(255, 255, 255),
            dim: rgb(153, 153, 153), // inactive
            error: rgb(255, 107, 128),
            success: rgb(78, 186, 101),
            warning: rgb(255, 193, 7),
            permission: rgb(177, 185, 249),
            plan_mode: rgb(72, 150, 140),
            suggestion: rgb(177, 185, 249),
            claude: rgb(215, 119, 87),
            diff_added: rgb(34, 92, 43),
            diff_removed: rgb(122, 41, 54),
            diff_added_word: rgb(56, 166, 96),
            diff_removed_word: rgb(179, 89, 107),
        }
    }

    /// Light theme — `theme.ts` `lightTheme`.
    #[must_use]
    pub const fn light() -> Theme {
        Theme {
            text: rgb(0, 0, 0),
            dim: rgb(102, 102, 102), // inactive
            error: rgb(171, 43, 63),
            success: rgb(44, 122, 57),
            warning: rgb(150, 108, 30),
            permission: rgb(87, 105, 247),
            plan_mode: rgb(0, 102, 102),
            suggestion: rgb(87, 105, 247),
            claude: rgb(215, 119, 87),
            diff_added: rgb(105, 219, 124),
            diff_removed: rgb(255, 168, 180),
            diff_added_word: rgb(47, 157, 68),
            diff_removed_word: rgb(209, 69, 75),
        }
    }

    /// Dark daltonized — `theme.ts` `darkDaltonizedTheme`.
    #[must_use]
    pub const fn dark_daltonized() -> Theme {
        Theme {
            text: rgb(255, 255, 255),
            dim: rgb(153, 153, 153),
            error: rgb(255, 102, 102),
            success: rgb(51, 153, 255), // blue-for-green
            warning: rgb(255, 204, 0),
            permission: rgb(153, 204, 255),
            plan_mode: rgb(102, 153, 153),
            suggestion: rgb(153, 204, 255),
            claude: rgb(255, 153, 51),
            diff_added: rgb(0, 68, 102),
            diff_removed: rgb(102, 0, 0),
            diff_added_word: rgb(0, 119, 179),
            diff_removed_word: rgb(179, 0, 0),
        }
    }

    /// Light daltonized — `theme.ts` `lightDaltonizedTheme`.
    #[must_use]
    pub const fn light_daltonized() -> Theme {
        Theme {
            text: rgb(0, 0, 0),
            dim: rgb(102, 102, 102),
            error: rgb(204, 0, 0),
            success: rgb(0, 102, 153), // blue-for-green
            warning: rgb(255, 153, 0),
            permission: rgb(51, 102, 255),
            plan_mode: rgb(51, 102, 102),
            suggestion: rgb(51, 102, 255),
            claude: rgb(255, 153, 51),
            diff_added: rgb(153, 204, 255),
            diff_removed: rgb(255, 204, 204),
            diff_added_word: rgb(51, 102, 204),
            diff_removed_word: rgb(153, 51, 51),
        }
    }

    /// Dark ANSI — `theme.ts` `darkAnsiTheme` (named colors only).
    #[must_use]
    pub fn dark_ansi() -> Theme {
        Theme {
            text: ansi("ansi:whiteBright"),
            dim: ansi("ansi:white"), // inactive
            error: ansi("ansi:redBright"),
            success: ansi("ansi:greenBright"),
            warning: ansi("ansi:yellowBright"),
            permission: ansi("ansi:blueBright"),
            plan_mode: ansi("ansi:cyanBright"),
            suggestion: ansi("ansi:blueBright"),
            claude: ansi("ansi:redBright"),
            diff_added: ansi("ansi:green"),
            diff_removed: ansi("ansi:red"),
            diff_added_word: ansi("ansi:greenBright"),
            diff_removed_word: ansi("ansi:redBright"),
        }
    }

    /// Light ANSI — `theme.ts` `lightAnsiTheme` (named colors only).
    #[must_use]
    pub fn light_ansi() -> Theme {
        Theme {
            text: ansi("ansi:black"),
            dim: ansi("ansi:blackBright"), // inactive
            error: ansi("ansi:red"),
            success: ansi("ansi:green"),
            warning: ansi("ansi:yellow"),
            permission: ansi("ansi:blue"),
            plan_mode: ansi("ansi:cyan"),
            suggestion: ansi("ansi:blue"),
            claude: ansi("ansi:redBright"),
            diff_added: ansi("ansi:green"),
            diff_removed: ansi("ansi:red"),
            diff_added_word: ansi("ansi:greenBright"),
            diff_removed_word: ansi("ansi:redBright"),
        }
    }
}

/// Registry lookup: resolve a [`ThemeName`] to its concrete palette.
#[must_use]
pub fn theme_for(name: ThemeName) -> Theme {
    match name {
        ThemeName::Dark => Theme::dark(),
        ThemeName::Light => Theme::light(),
        ThemeName::DarkDaltonized => Theme::dark_daltonized(),
        ThemeName::LightDaltonized => Theme::light_daltonized(),
        ThemeName::DarkAnsi => Theme::dark_ansi(),
        ThemeName::LightAnsi => Theme::light_ansi(),
    }
}

/// Locked color constants for assistant / user / error / dim text.
///
/// (M7-15) Shim — migrated to [`Theme`] in M7-15. These consts now delegate to
/// [`Theme::dark`] values so any straggler M6 call site compiles unchanged.
/// Production render paths read from the active `AppState.theme` instead.
pub struct TuiTheme;

impl TuiTheme {
    /// Assistant body text color — `Theme::dark().claude`.
    pub const ASSISTANT: StyleColor = Theme::dark().claude;
    /// User prompt color — terminal default foreground (no theme key; claude
    /// renders user text uncolored).
    pub const USER: StyleColor = StyleColor::Default;
    /// Error/system-error text — `Theme::dark().error`.
    pub const ERROR: StyleColor = Theme::dark().error;
    /// Success text (resolved tool-use dot) — `Theme::dark().success`.
    pub const SUCCESS: StyleColor = Theme::dark().success;
    /// Dim text (system hints, footnotes) — `Theme::dark().dim`.
    pub const DIM: StyleColor = Theme::dark().dim;
    /// Warning text (e.g. an empty-state line) — `Theme::dark().warning`.
    pub const WARNING: StyleColor = Theme::dark().warning;
}
