use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::error::Error;
use std::fmt::{self, Formatter};
use std::mem;
use std::path::PathBuf;
use std::rc::Rc;

use log::{error, warn};
use serde::de::{Error as SerdeError, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use unicode_width::UnicodeWidthChar;
use winit::keyboard::{Key, ModifiersState};

use alacritty_config::SerdeReplace;
use alacritty_config_derive::{ConfigDeserialize, SerdeReplace};
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::term::search::RegexSearch;
use alacritty_terminal::tty::{Options as PtyOptions, Shell};

use crate::config::LOG_TARGET_CONFIG;
use crate::config::bell::BellConfig;
use crate::config::bindings::{
    self, Action, Binding, BindingKey, KeyBinding, KeyLocation, ModeWrapper, ModsWrapper,
    MouseBinding,
};
use crate::config::color::Colors;
use crate::config::cursor::Cursor;
use crate::config::debug::Debug;
use crate::config::font::Font;
use crate::config::general::General;
use crate::config::mouse::Mouse;
use crate::config::scrolling::Scrolling;
use crate::config::selection::Selection;
use crate::config::terminal::Terminal;
use crate::config::window::{
    GradientConfig, GradientDirection, TabBarConfig, TabBarStyle, ThemePreset, WindowConfig,
};

/// Regex used for the default URL hint.
#[rustfmt::skip]
const URL_REGEX: &str = "(ipfs:|ipns:|magnet:|mailto:|gemini://|gopher://|https://|http://|news:|file:|git://|ssh:|ftp://)\
                         [^\u{0000}-\u{001F}\u{007F}-\u{009F}<>\"\\s{-}\\^⟨⟩`\\\\]+";

#[derive(ConfigDeserialize, Serialize, Default, Clone, Debug, PartialEq)]
pub struct UiConfig {
    /// Miscellaneous configuration options.
    pub general: General,

    /// Extra environment variables.
    pub env: HashMap<String, String>,

    /// How much scrolling history to keep.
    pub scrolling: Scrolling,

    /// Cursor configuration.
    pub cursor: Cursor,

    /// Selection configuration.
    pub selection: Selection,

    /// Font configuration.
    pub font: Font,

    /// Window configuration.
    pub window: WindowConfig,

    /// Mouse configuration.
    pub mouse: Mouse,

    /// Debug options.
    pub debug: Debug,

    /// Bell configuration.
    pub bell: BellConfig,

    /// RGB values for colors.
    pub colors: Colors,

    /// Path where config was loaded from.
    #[config(skip)]
    #[serde(skip_serializing)]
    pub config_paths: Vec<PathBuf>,

    /// Regex hints for interacting with terminal content.
    pub hints: Hints,

    /// Config for the alacritty_terminal itself.
    pub terminal: Terminal,

    /// Keyboard configuration.
    keyboard: Keyboard,

    /// Path to a shell program to run on startup.
    #[config(deprecated = "use terminal.shell instead")]
    shell: Option<Program>,

    /// Configuration file imports.
    ///
    /// This is never read since the field is directly accessed through the config's
    /// [`toml::Value`], but still present to prevent unused field warnings.
    #[config(deprecated = "use general.import instead")]
    import: Option<Vec<String>>,

    /// Shell startup directory.
    #[config(deprecated = "use general.working_directory instead")]
    working_directory: Option<PathBuf>,

    /// Live config reload.
    #[config(deprecated = "use general.live_config_reload instead")]
    live_config_reload: Option<bool>,

    /// Offer IPC through a unix socket.
    #[cfg(unix)]
    #[config(deprecated = "use general.ipc_socket instead")]
    pub ipc_socket: Option<bool>,
}

impl UiConfig {
    pub fn apply_theme_preset(&mut self) {
        let Some(theme) = self.window.theme_preset else {
            return;
        };

        match theme {
            ThemePreset::TokyoNight => {
                self.colors.primary.background = rgb(0x1a, 0x1b, 0x26);
                self.colors.primary.foreground = rgb(0xc0, 0xca, 0xf5);
                self.colors.primary.bright_foreground = Some(rgb(0xc0, 0xca, 0xf5));
                self.colors.normal = normal_colors(
                    rgb(0x15, 0x19, 0x2d),
                    rgb(0xf7, 0x76, 0x8e),
                    rgb(0x9e, 0xce, 0x6a),
                    rgb(0xe0, 0xaf, 0x68),
                    rgb(0x7a, 0xa2, 0xf7),
                    rgb(0xbb, 0x9a, 0xf7),
                    rgb(0x7d, 0xcf, 0xff),
                    rgb(0xa9, 0xb1, 0xd6),
                );
                self.colors.bright = bright_colors(
                    rgb(0x41, 0x49, 0x68),
                    rgb(0xff, 0x89, 0x9d),
                    rgb(0xb9, 0xf2, 0x7c),
                    rgb(0xff, 0xc7, 0x77),
                    rgb(0x9a, 0xb8, 0xff),
                    rgb(0xce, 0xb0, 0xff),
                    rgb(0xa4, 0xda, 0xff),
                    rgb(0xc0, 0xca, 0xf5),
                );
                self.window.background_gradient = Some(GradientConfig {
                    start: rgb(0x1a, 0x1b, 0x26),
                    end: rgb(0x24, 0x28, 0x3b),
                    direction: GradientDirection::Diagonal,
                });
                self.window.tab_bar = TabBarConfig {
                    style: TabBarStyle::Rounded,
                    active_color: rgb(0x7a, 0xa2, 0xf7),
                    inactive_color: rgb(0x41, 0x49, 0x68),
                    text_color: rgb(0xc0, 0xca, 0xf5),
                    height: 36,
                };
            },
            ThemePreset::CatppuccinMocha => {
                self.colors.primary.background = rgb(0x1e, 0x1e, 0x2e);
                self.colors.primary.foreground = rgb(0xcd, 0xd6, 0xf4);
                self.colors.primary.bright_foreground = Some(rgb(0xf5, 0xe0, 0xdc));
                self.colors.normal = normal_colors(
                    rgb(0x45, 0x47, 0x5a),
                    rgb(0xf3, 0x8b, 0xa8),
                    rgb(0xa6, 0xe3, 0xa1),
                    rgb(0xf9, 0xe2, 0xaf),
                    rgb(0x89, 0xb4, 0xfa),
                    rgb(0xf5, 0xc2, 0xe7),
                    rgb(0x94, 0xe2, 0xd5),
                    rgb(0xba, 0xc2, 0xde),
                );
                self.colors.bright = bright_colors(
                    rgb(0x58, 0x5b, 0x70),
                    rgb(0xf6, 0x9c, 0xb5),
                    rgb(0xb7, 0xef, 0xb2),
                    rgb(0xfa, 0xe8, 0xc3),
                    rgb(0x9f, 0xc5, 0xfb),
                    rgb(0xf7, 0xcf, 0xeb),
                    rgb(0xaa, 0xe9, 0xdd),
                    rgb(0xda, 0xe0, 0xf7),
                );
                self.window.background_gradient = Some(GradientConfig {
                    start: rgb(0x1e, 0x1e, 0x2e),
                    end: rgb(0x31, 0x32, 0x44),
                    direction: GradientDirection::Vertical,
                });
                self.window.tab_bar = TabBarConfig {
                    style: TabBarStyle::Rounded,
                    active_color: rgb(0x89, 0xb4, 0xfa),
                    inactive_color: rgb(0x45, 0x47, 0x5a),
                    text_color: rgb(0xcd, 0xd6, 0xf4),
                    height: 36,
                };
            },
            ThemePreset::Nord => {
                self.colors.primary.background = rgb(0x2e, 0x34, 0x40);
                self.colors.primary.foreground = rgb(0xd8, 0xde, 0xe9);
                self.colors.primary.bright_foreground = Some(rgb(0xec, 0xef, 0xf4));
                self.colors.normal = normal_colors(
                    rgb(0x3b, 0x42, 0x52),
                    rgb(0xbf, 0x61, 0x6a),
                    rgb(0xa3, 0xbe, 0x8c),
                    rgb(0xeb, 0xcb, 0x8b),
                    rgb(0x81, 0xa1, 0xc1),
                    rgb(0xb4, 0x8e, 0xad),
                    rgb(0x88, 0xc0, 0xd0),
                    rgb(0xe5, 0xe9, 0xf0),
                );
                self.colors.bright = bright_colors(
                    rgb(0x4c, 0x56, 0x6a),
                    rgb(0xcf, 0x7b, 0x84),
                    rgb(0xb4, 0xcf, 0x9d),
                    rgb(0xf2, 0xd4, 0xa0),
                    rgb(0x92, 0xb3, 0xd3),
                    rgb(0xc3, 0x9f, 0xbc),
                    rgb(0x99, 0xcd, 0xdc),
                    rgb(0xec, 0xef, 0xf4),
                );
                self.window.background_gradient = Some(GradientConfig {
                    start: rgb(0x2e, 0x34, 0x40),
                    end: rgb(0x3b, 0x42, 0x52),
                    direction: GradientDirection::Diagonal,
                });
                self.window.tab_bar = TabBarConfig {
                    style: TabBarStyle::Rounded,
                    active_color: rgb(0x88, 0xc0, 0xd0),
                    inactive_color: rgb(0x4c, 0x56, 0x6a),
                    text_color: rgb(0xd8, 0xde, 0xe9),
                    height: 36,
                };
            },
            ThemePreset::Dracula => {
                self.colors.primary.background = rgb(0x28, 0x2a, 0x36);
                self.colors.primary.foreground = rgb(0xf8, 0xf8, 0xf2);
                self.colors.primary.bright_foreground = Some(rgb(0xff, 0xff, 0xff));
                self.colors.normal = normal_colors(
                    rgb(0x21, 0x23, 0x2c),
                    rgb(0xff, 0x55, 0x55),
                    rgb(0x50, 0xfa, 0x7b),
                    rgb(0xf1, 0xfa, 0x8c),
                    rgb(0xbd, 0x93, 0xf9),
                    rgb(0xff, 0x79, 0xc6),
                    rgb(0x8b, 0xe9, 0xfd),
                    rgb(0xf8, 0xf8, 0xf2),
                );
                self.colors.bright = bright_colors(
                    rgb(0x62, 0x72, 0xa4),
                    rgb(0xff, 0x6e, 0x6e),
                    rgb(0x69, 0xff, 0x8f),
                    rgb(0xf6, 0xff, 0xa5),
                    rgb(0xca, 0xa9, 0xff),
                    rgb(0xff, 0x92, 0xd0),
                    rgb(0xa4, 0xf0, 0xff),
                    rgb(0xff, 0xff, 0xff),
                );
                self.window.background_gradient = Some(GradientConfig {
                    start: rgb(0x28, 0x2a, 0x36),
                    end: rgb(0x44, 0x47, 0x5a),
                    direction: GradientDirection::Vertical,
                });
                self.window.tab_bar = TabBarConfig {
                    style: TabBarStyle::Rounded,
                    active_color: rgb(0xbd, 0x93, 0xf9),
                    inactive_color: rgb(0x44, 0x47, 0x5a),
                    text_color: rgb(0xf8, 0xf8, 0xf2),
                    height: 36,
                };
            },
            ThemePreset::OneDark => {
                self.colors.primary.background = rgb(0x28, 0x2c, 0x34);
                self.colors.primary.foreground = rgb(0xab, 0xb2, 0xbf);
                self.colors.primary.bright_foreground = Some(rgb(0xc8, 0xcc, 0xd4));
                self.colors.normal = normal_colors(
                    rgb(0x1e, 0x21, 0x27),
                    rgb(0xe0, 0x6c, 0x75),
                    rgb(0x98, 0xc3, 0x79),
                    rgb(0xe5, 0xc0, 0x7b),
                    rgb(0x61, 0xaf, 0xef),
                    rgb(0xc6, 0x78, 0xdd),
                    rgb(0x56, 0xb6, 0xc2),
                    rgb(0xdc, 0xe0, 0xe8),
                );
                self.colors.bright = bright_colors(
                    rgb(0x5c, 0x63, 0x70),
                    rgb(0xe8, 0x7d, 0x86),
                    rgb(0xa8, 0xcf, 0x89),
                    rgb(0xeb, 0xcb, 0x8e),
                    rgb(0x74, 0xbc, 0xf4),
                    rgb(0xd0, 0x8d, 0xe5),
                    rgb(0x68, 0xc5, 0xd0),
                    rgb(0xe5, 0xe9, 0xf0),
                );
                self.window.background_gradient = Some(GradientConfig {
                    start: rgb(0x28, 0x2c, 0x34),
                    end: rgb(0x21, 0x24, 0x2b),
                    direction: GradientDirection::Diagonal,
                });
                self.window.tab_bar = TabBarConfig {
                    style: TabBarStyle::Rounded,
                    active_color: rgb(0x61, 0xaf, 0xef),
                    inactive_color: rgb(0x3e, 0x44, 0x51),
                    text_color: rgb(0xab, 0xb2, 0xbf),
                    height: 36,
                };
            },
        }
    }

    /// Derive [`TermConfig`] from the config.
    pub fn term_options(&self) -> TermConfig {
        TermConfig {
            semantic_escape_chars: self.selection.semantic_escape_chars.clone(),
            scrolling_history: self.scrolling.history() as usize,
            vi_mode_cursor_style: self.cursor.vi_mode_style(),
            default_cursor_style: self.cursor.style(),
            osc52: self.terminal.osc52.0,
            kitty_keyboard: true,
        }
    }

    /// Derive [`PtyOptions`] from the config.
    pub fn pty_config(&self) -> PtyOptions {
        let shell = self.terminal.shell.clone().or_else(|| self.shell.clone()).map(Into::into);
        let working_directory =
            self.working_directory.clone().or_else(|| self.general.working_directory.clone());
        PtyOptions {
            working_directory,
            shell,
            drain_on_exit: false,
            env: HashMap::new(),
            #[cfg(target_os = "windows")]
            escape_args: false,
        }
    }

    #[inline]
    pub fn window_opacity(&self) -> f32 {
        self.window.opacity.as_f32()
    }

    #[inline]
    pub fn key_bindings(&self) -> &[KeyBinding] {
        &self.keyboard.bindings.0
    }

    #[inline]
    pub fn mouse_bindings(&self) -> &[MouseBinding] {
        &self.mouse.bindings.0
    }

    #[inline]
    pub fn live_config_reload(&self) -> bool {
        self.live_config_reload.unwrap_or(self.general.live_config_reload)
    }

    #[cfg(unix)]
    #[inline]
    pub fn ipc_socket(&self) -> bool {
        self.ipc_socket.unwrap_or(self.general.ipc_socket)
    }
}

fn rgb(r: u8, g: u8, b: u8) -> crate::display::color::Rgb {
    crate::display::color::Rgb::new(r, g, b)
}

fn normal_colors(
    black: crate::display::color::Rgb,
    red: crate::display::color::Rgb,
    green: crate::display::color::Rgb,
    yellow: crate::display::color::Rgb,
    blue: crate::display::color::Rgb,
    magenta: crate::display::color::Rgb,
    cyan: crate::display::color::Rgb,
    white: crate::display::color::Rgb,
) -> crate::config::color::NormalColors {
    crate::config::color::NormalColors { black, red, green, yellow, blue, magenta, cyan, white }
}

fn bright_colors(
    black: crate::display::color::Rgb,
    red: crate::display::color::Rgb,
    green: crate::display::color::Rgb,
    yellow: crate::display::color::Rgb,
    blue: crate::display::color::Rgb,
    magenta: crate::display::color::Rgb,
    cyan: crate::display::color::Rgb,
    white: crate::display::color::Rgb,
) -> crate::config::color::BrightColors {
    crate::config::color::BrightColors { black, red, green, yellow, blue, magenta, cyan, white }
}

/// Keyboard configuration.
#[derive(ConfigDeserialize, Serialize, Default, Clone, Debug, PartialEq)]
struct Keyboard {
    /// Keybindings.
    #[serde(skip_serializing)]
    bindings: KeyBindings,
}

#[derive(SerdeReplace, Clone, Debug, PartialEq, Eq)]
struct KeyBindings(Vec<KeyBinding>);

impl Default for KeyBindings {
    fn default() -> Self {
        Self(bindings::default_key_bindings())
    }
}

impl<'de> Deserialize<'de> for KeyBindings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self(deserialize_bindings(deserializer, Self::default().0)?))
    }
}

pub fn deserialize_bindings<'a, D, T>(
    deserializer: D,
    mut default: Vec<Binding<T>>,
) -> Result<Vec<Binding<T>>, D::Error>
where
    D: Deserializer<'a>,
    T: Clone + Eq,
    Binding<T>: Deserialize<'a>,
{
    let values = Vec::<toml::Value>::deserialize(deserializer)?;

    // Skip all invalid values.
    let mut bindings = Vec::with_capacity(values.len());
    for value in values {
        match Binding::<T>::deserialize(value) {
            Ok(binding) => bindings.push(binding),
            Err(err) => {
                error!(target: LOG_TARGET_CONFIG, "Config error: {err}; ignoring binding");
            },
        }
    }

    // Remove matching default bindings.
    for binding in bindings.iter() {
        default.retain(|b| !b.triggers_match(binding));
    }

    bindings.extend(default);

    Ok(bindings)
}

/// A delta for a point in a 2 dimensional plane.
#[derive(ConfigDeserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Delta<T: Default> {
    /// Horizontal change.
    pub x: T,
    /// Vertical change.
    pub y: T,
}

/// Regex terminal hints.
#[derive(ConfigDeserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Hints {
    /// Characters for the hint labels.
    alphabet: HintsAlphabet,

    /// All configured terminal hints.
    pub enabled: Vec<Rc<Hint>>,
}

impl Default for Hints {
    fn default() -> Self {
        // Add URL hint by default when no other hint is present.
        let pattern = LazyRegexVariant::Pattern(String::from(URL_REGEX));
        let regex = LazyRegex(Rc::new(RefCell::new(pattern)));
        let content = HintContent::new(Some(regex), true);

        #[cfg(not(any(target_os = "macos", windows)))]
        let action = HintAction::Command(Program::Just(String::from("xdg-open")));
        #[cfg(target_os = "macos")]
        let action = HintAction::Command(Program::Just(String::from("open")));
        #[cfg(windows)]
        let action = HintAction::Command(Program::WithArgs {
            program: String::from("cmd"),
            args: vec!["/c".to_string(), "start".to_string(), "".to_string()],
        });

        Self {
            enabled: vec![Rc::new(Hint {
                content,
                action,
                persist: false,
                post_processing: true,
                mouse: Some(HintMouse { enabled: true, mods: Default::default() }),
                binding: Some(HintBinding {
                    key: BindingKey::Keycode {
                        key: Key::Character("o".into()),
                        location: KeyLocation::Standard,
                    },
                    mods: ModsWrapper(ModifiersState::SHIFT | ModifiersState::CONTROL),
                    cache: Default::default(),
                    mode: Default::default(),
                }),
            })],
            alphabet: Default::default(),
        }
    }
}

impl Hints {
    /// Characters for the hint labels.
    pub fn alphabet(&self) -> &str {
        &self.alphabet.0
    }
}

#[derive(SerdeReplace, Serialize, Clone, Debug, PartialEq, Eq)]
struct HintsAlphabet(String);

impl Default for HintsAlphabet {
    fn default() -> Self {
        Self(String::from("jfkdls;ahgurieowpq"))
    }
}

impl<'de> Deserialize<'de> for HintsAlphabet {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;

        let mut character_count = 0;
        for character in value.chars() {
            if character.width() != Some(1) {
                return Err(D::Error::custom("characters must be of width 1"));
            }
            character_count += 1;
        }

        if character_count < 2 {
            return Err(D::Error::custom("must include at last 2 characters"));
        }

        Ok(Self(value))
    }
}

/// Built-in actions for hint mode.
#[derive(ConfigDeserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub enum HintInternalAction {
    /// Copy the text to the clipboard.
    Copy,
    /// Write the text to the PTY/search.
    Paste,
    /// Select the text matching the hint.
    Select,
    /// Move the vi mode cursor to the beginning of the hint.
    MoveViModeCursor,
}

/// Actions for hint bindings.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub enum HintAction {
    /// Built-in hint action.
    #[serde(rename = "action")]
    Action(HintInternalAction),

    /// Command the text will be piped to.
    #[serde(rename = "command")]
    Command(Program),
}

/// Hint configuration.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Hint {
    /// Regex for finding matches.
    #[serde(flatten)]
    pub content: HintContent,

    /// Action executed when this hint is triggered.
    #[serde(flatten)]
    pub action: HintAction,

    /// Hint text post processing.
    #[serde(default)]
    pub post_processing: bool,

    /// Persist hints after selection.
    #[serde(default)]
    pub persist: bool,

    /// Hint mouse highlighting.
    pub mouse: Option<HintMouse>,

    /// Binding required to search for this hint.
    #[serde(skip_serializing)]
    pub binding: Option<HintBinding>,
}

#[derive(Serialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct HintContent {
    /// Regex for finding matches.
    pub regex: Option<LazyRegex>,

    /// Escape sequence hyperlinks.
    pub hyperlinks: bool,
}

impl HintContent {
    pub fn new(regex: Option<LazyRegex>, hyperlinks: bool) -> Self {
        Self { regex, hyperlinks }
    }
}

impl<'de> Deserialize<'de> for HintContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct HintContentVisitor;
        impl<'a> Visitor<'a> for HintContentVisitor {
            type Value = HintContent;

            fn expecting(&self, f: &mut Formatter<'_>) -> fmt::Result {
                f.write_str("a mapping")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'a>,
            {
                let mut content = Self::Value::default();

                while let Some((key, value)) = map.next_entry::<String, toml::Value>()? {
                    match key.as_str() {
                        "regex" => match Option::<LazyRegex>::deserialize(value) {
                            Ok(regex) => content.regex = regex,
                            Err(err) => {
                                error!(
                                    target: LOG_TARGET_CONFIG,
                                    "Config error: hint's regex: {err}"
                                );
                            },
                        },
                        "hyperlinks" => match bool::deserialize(value) {
                            Ok(hyperlink) => content.hyperlinks = hyperlink,
                            Err(err) => {
                                error!(
                                    target: LOG_TARGET_CONFIG,
                                    "Config error: hint's hyperlinks: {err}"
                                );
                            },
                        },
                        "command" | "action" => (),
                        key => warn!(target: LOG_TARGET_CONFIG, "Unrecognized hint field: {key}"),
                    }
                }

                // Require at least one of hyperlinks or regex trigger hint matches.
                if content.regex.is_none() && !content.hyperlinks {
                    return Err(M::Error::custom(
                        "Config error: At least one of the hint's regex or hint's hyperlinks must \
                         be set",
                    ));
                }

                Ok(content)
            }
        }

        deserializer.deserialize_any(HintContentVisitor)
    }
}

/// Binding for triggering a keyboard hint.
#[derive(Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HintBinding {
    pub key: BindingKey,
    #[serde(default)]
    pub mods: ModsWrapper,
    #[serde(default)]
    pub mode: ModeWrapper,

    /// Cache for on-demand [`HintBinding`] to [`KeyBinding`] conversion.
    #[serde(skip)]
    cache: OnceCell<KeyBinding>,
}

impl HintBinding {
    /// Get the key binding for a hint.
    pub fn key_binding(&self, hint: &Rc<Hint>) -> &KeyBinding {
        self.cache.get_or_init(|| KeyBinding {
            trigger: self.key.clone(),
            mods: self.mods.0,
            mode: self.mode.mode,
            notmode: self.mode.not_mode,
            action: Action::Hint(hint.clone()),
        })
    }
}

impl fmt::Debug for HintBinding {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("HintBinding")
            .field("key", &self.key)
            .field("mods", &self.mods)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

/// Hint mouse highlighting.
#[derive(ConfigDeserialize, Serialize, Default, Copy, Clone, Debug, PartialEq, Eq)]
pub struct HintMouse {
    /// Hint mouse highlighting availability.
    pub enabled: bool,

    /// Required mouse modifiers for hint highlighting.
    #[serde(skip_serializing)]
    pub mods: ModsWrapper,
}

/// Lazy regex with interior mutability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LazyRegex(Rc<RefCell<LazyRegexVariant>>);

impl LazyRegex {
    /// Execute a function with the compiled regex DFAs as parameter.
    pub fn with_compiled<T, F>(&self, f: F) -> Option<T>
    where
        F: FnMut(&mut RegexSearch) -> T,
    {
        self.0.borrow_mut().compiled().map(f)
    }
}

impl<'de> Deserialize<'de> for LazyRegex {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let regex = LazyRegexVariant::Pattern(String::deserialize(deserializer)?);
        Ok(Self(Rc::new(RefCell::new(regex))))
    }
}

impl Serialize for LazyRegex {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let variant = self.0.borrow();
        let regex = match &*variant {
            LazyRegexVariant::Compiled(regex, _) => regex,
            LazyRegexVariant::Uncompilable(regex) => regex,
            LazyRegexVariant::Pattern(regex) => regex,
        };
        serializer.serialize_str(regex)
    }
}

/// Regex which is compiled on demand, to avoid expensive computations at startup.
#[derive(Clone, Debug)]
pub enum LazyRegexVariant {
    Compiled(String, Box<RegexSearch>),
    Pattern(String),
    Uncompilable(String),
}

impl LazyRegexVariant {
    /// Get a reference to the compiled regex.
    ///
    /// If the regex is not already compiled, this will compile the DFAs and store them for future
    /// access.
    fn compiled(&mut self) -> Option<&mut RegexSearch> {
        // Check if the regex has already been compiled.
        let regex = match self {
            Self::Compiled(_, regex_search) => return Some(regex_search),
            Self::Uncompilable(_) => return None,
            Self::Pattern(regex) => mem::take(regex),
        };

        // Compile the regex.
        let regex_search = match RegexSearch::new(&regex) {
            Ok(regex_search) => regex_search,
            Err(err) => {
                error!("could not compile hint regex: {err}");
                *self = Self::Uncompilable(regex);
                return None;
            },
        };
        *self = Self::Compiled(regex, Box::new(regex_search));

        // Return a reference to the compiled DFAs.
        match self {
            Self::Compiled(_, dfas) => Some(dfas),
            _ => unreachable!(),
        }
    }
}

impl PartialEq for LazyRegexVariant {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Pattern(regex), Self::Pattern(other_regex)) => regex == other_regex,
            _ => false,
        }
    }
}
impl Eq for LazyRegexVariant {}

/// Wrapper around f32 that represents a percentage value between 0.0 and 1.0.
#[derive(SerdeReplace, Serialize, Clone, Copy, Debug, PartialEq)]
pub struct Percentage(f32);

impl Default for Percentage {
    fn default() -> Self {
        Percentage(1.0)
    }
}

impl Percentage {
    pub fn new(value: f32) -> Self {
        Percentage(value.clamp(0., 1.))
    }

    pub fn as_f32(self) -> f32 {
        self.0
    }
}

impl<'de> Deserialize<'de> for Percentage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Percentage::new(f32::deserialize(deserializer)?))
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged, deny_unknown_fields)]
pub enum Program {
    Just(String),
    WithArgs {
        program: String,
        #[serde(default)]
        args: Vec<String>,
    },
}

impl Program {
    pub fn program(&self) -> &str {
        match self {
            Program::Just(program) => program,
            Program::WithArgs { program, .. } => program,
        }
    }

    pub fn args(&self) -> &[String] {
        match self {
            Program::Just(_) => &[],
            Program::WithArgs { args, .. } => args,
        }
    }
}

impl From<Program> for Shell {
    fn from(value: Program) -> Self {
        match value {
            Program::Just(program) => Shell::new(program, Vec::new()),
            Program::WithArgs { program, args } => Shell::new(program, args),
        }
    }
}

impl SerdeReplace for Program {
    fn replace(&mut self, value: toml::Value) -> Result<(), Box<dyn Error>> {
        *self = Self::deserialize(value)?;

        Ok(())
    }
}

pub(crate) struct StringVisitor;
impl serde::de::Visitor<'_> for StringVisitor {
    type Value = String;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a string")
    }

    fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(s.to_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alacritty_terminal::term::test::mock_term;

    use crate::display::hint::visible_regex_match_iter;

    #[test]
    fn positive_url_parsing_regex_test() {
        for regular_url in [
            "ipfs:s0mEhAsh",
            "ipns:an0TherHash1234",
            "magnet:?xt=urn:btih:L0UDHA5H12",
            "mailto:example@example.org",
            "gemini://gemini.example.org/",
            "gopher://gopher.example.org",
            "https://www.example.org",
            "http://example.org",
            "news:some.news.portal",
            "file:///C:/Windows/",
            "file:/home/user/whatever",
            "git://github.com/user/repo.git",
            "ssh:git@github.com:user/repo.git",
            "ftp://ftp.example.org",
        ] {
            let term = mock_term(regular_url);
            let mut regex = RegexSearch::new(URL_REGEX).unwrap();
            let matches = visible_regex_match_iter(&term, &mut regex).collect::<Vec<_>>();
            assert_eq!(
                matches.len(),
                1,
                "Should have exactly one match url {regular_url}, but instead got: {matches:?}"
            )
        }
    }

    #[test]
    fn negative_url_parsing_regex_test() {
        for url_like in [
            "http::trace::on_request::log_parameters",
            "http//www.example.org",
            "/user:example.org",
            "mailto: example@example.org",
            "http://<script>alert('xss')</script>",
            "mailto:",
        ] {
            let term = mock_term(url_like);
            let mut regex = RegexSearch::new(URL_REGEX).unwrap();
            let matches = visible_regex_match_iter(&term, &mut regex).collect::<Vec<_>>();
            assert!(
                matches.is_empty(),
                "Should not match url in string {url_like}, but instead got: {matches:?}"
            )
        }
    }
}
