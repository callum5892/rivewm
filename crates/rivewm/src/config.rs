//! The TOML config file: gaps, key bindings and window rules.
//!
//! The defaults *are* `default_config.toml`, compiled in. It's written out
//! for the user on first run, and any section their file leaves out falls
//! back to it, so the two can't drift apart.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use regex::Regex;
use rivewm_core::{Command, Gaps, Layout};
use rivewm_platform::{BorderColor, Hotkey, WindowInfo};
use serde::Deserialize;
use tracing::info;

pub const DEFAULT_CONFIG: &str = include_str!("default_config.toml");

#[derive(Debug)]
pub struct Config {
    pub gaps: Gaps,
    /// Keep floating windows always on top.
    pub floating_on_top: bool,
    /// Layout for newly created workspaces.
    pub default_layout: Layout,
    pub border: Border,
    /// Resize neighbours while an edge is being dragged, not just on release.
    pub live_resize: bool,
    /// Focus the window under the mouse as it moves.
    pub focus_follows_mouse: bool,
    pub bindings: Vec<(Hotkey, Command)>,
    pub rules: Vec<Rule>,
}

/// Window border colours, applied to every managed window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Border {
    pub enabled: bool,
    pub focused: BorderColor,
    pub unfocused: BorderColor,
}

/// `"#rrggbb"`, `"default"` or `"none"`.
fn parse_color(text: &str) -> Result<BorderColor> {
    match text {
        "default" => Ok(BorderColor::Default),
        "none" => Ok(BorderColor::Hidden),
        _ => {
            let hex = text
                .strip_prefix('#')
                .filter(|h| h.len() == 6)
                .and_then(|h| u32::from_str_radix(h, 16).ok());
            match hex {
                Some(v) => Ok(BorderColor::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)),
                None => bail!("bad colour `{text}`: use \"#rrggbb\", \"default\" or \"none\""),
            }
        }
    }
}

#[derive(Debug)]
pub struct Rule {
    /// Lower-cased executable name.
    process: Option<String>,
    class: Option<String>,
    title: Option<Regex>,
    pub action: Option<RuleAction>,
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    Tile,
    Float,
    Ignore,
}

impl Rule {
    fn matches(&self, w: &WindowInfo) -> bool {
        let process = w.process.as_deref().unwrap_or_default();
        self.process
            .as_ref()
            .is_none_or(|p| p.eq_ignore_ascii_case(process))
            && self.class.as_ref().is_none_or(|c| *c == w.class)
            && self.title.as_ref().is_none_or(|t| t.is_match(&w.title))
    }
}

impl Config {
    /// The first rule matching a window, if any.
    pub fn rule_for(&self, window: &WindowInfo) -> Option<&Rule> {
        self.rules.iter().find(|r| r.matches(window))
    }
}

/// `~\.config\rivewm\config.toml`.
pub fn default_path() -> PathBuf {
    std::env::home_dir()
        .unwrap_or_default()
        .join(".config")
        .join("rivewm")
        .join("config.toml")
}

/// Reads the config at `path`, first writing the defaults there if it
/// doesn't exist.
pub fn load(path: &Path) -> Result<Config> {
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("couldn't create {}", dir.display()))?;
        }
        std::fs::write(path, DEFAULT_CONFIG)
            .with_context(|| format!("couldn't write {}", path.display()))?;
        info!(path = %path.display(), "wrote default config");
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("couldn't read {}", path.display()))?;
    parse(&text).with_context(|| format!("invalid config {}", path.display()))
}

// ---- Raw TOML shape ---------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    gaps: Option<RawGaps>,
    border: Option<RawBorder>,
    resize: Option<RawResize>,
    focus: Option<RawFocus>,
    floating: Option<RawFloating>,
    layout: Option<RawLayout>,
    keybindings: Option<BTreeMap<String, String>>,
    rules: Option<Vec<RawRule>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGaps {
    inner: Option<i32>,
    outer: Option<i32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFocus {
    follows_mouse: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResize {
    live: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawBorder {
    enabled: Option<bool>,
    focused: Option<String>,
    unfocused: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLayout {
    default: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFloating {
    on_top: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    process: Option<String>,
    class: Option<String>,
    title: Option<String>,
    action: Option<RuleAction>,
    workspace: Option<String>,
}

/// Parses config text, filling anything it leaves out from the defaults.
pub fn parse(text: &str) -> Result<Config> {
    let defaults: RawConfig = toml::from_str(DEFAULT_CONFIG).expect("built-in config is valid");
    let user: RawConfig = toml::from_str(text)?;

    let default_gaps = defaults.gaps.expect("built-in config has [gaps]");
    let user_gaps = user.gaps.unwrap_or(RawGaps {
        inner: None,
        outer: None,
    });
    let gaps = Gaps {
        inner: user_gaps.inner.or(default_gaps.inner).unwrap_or(0),
        outer: user_gaps.outer.or(default_gaps.outer).unwrap_or(0),
    };
    if gaps.inner < 0 || gaps.outer < 0 {
        bail!("gaps can't be negative");
    }

    let default_layout: Layout = user
        .layout
        .and_then(|l| l.default)
        .or(defaults.layout.and_then(|l| l.default))
        .unwrap_or_default()
        .parse()
        .map_err(anyhow::Error::msg)
        .context("in [layout]")?;

    let user_border = user.border.unwrap_or_default();
    let default_border = defaults.border.unwrap_or_default();
    let color = |user: Option<String>, default: Option<String>, key: &str| {
        let text = user.or(default).unwrap_or_else(|| "default".into());
        parse_color(&text).with_context(|| format!("in [border] {key}"))
    };
    let border = Border {
        enabled: user_border
            .enabled
            .or(default_border.enabled)
            .unwrap_or(false),
        focused: color(user_border.focused, default_border.focused, "focused")?,
        unfocused: color(user_border.unfocused, default_border.unfocused, "unfocused")?,
    };

    let focus_follows_mouse = user
        .focus
        .and_then(|f| f.follows_mouse)
        .or(defaults.focus.and_then(|f| f.follows_mouse))
        .unwrap_or(false);

    let live_resize = user
        .resize
        .and_then(|r| r.live)
        .or(defaults.resize.and_then(|r| r.live))
        .unwrap_or(true);

    let floating_on_top = user
        .floating
        .and_then(|f| f.on_top)
        .or(defaults.floating.and_then(|f| f.on_top))
        .unwrap_or(false);

    let keybindings = user
        .keybindings
        .or(defaults.keybindings)
        .unwrap_or_default();
    let mut bindings = Vec::with_capacity(keybindings.len());
    for (key, command) in &keybindings {
        let hotkey: Hotkey = key.parse().context("in [keybindings]")?;
        let command: Command = command
            .parse()
            .with_context(|| format!("in [keybindings] for \"{key}\""))?;
        bindings.push((hotkey, command));
    }

    let rules = user
        .rules
        .or(defaults.rules)
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(i, raw)| rule(raw).with_context(|| format!("in rule #{}", i + 1)))
        .collect::<Result<_>>()?;

    Ok(Config {
        gaps,
        floating_on_top,
        default_layout,
        border,
        live_resize,
        focus_follows_mouse,
        bindings,
        rules,
    })
}

fn rule(raw: RawRule) -> Result<Rule> {
    if raw.process.is_none() && raw.class.is_none() && raw.title.is_none() {
        bail!("needs at least one of `process`, `class` or `title` to match on");
    }
    if raw.action.is_none() && raw.workspace.is_none() {
        bail!("needs an `action` or a `workspace`");
    }
    let title = raw
        .title
        .map(|t| Regex::new(&t).with_context(|| format!("bad title regex `{t}`")))
        .transpose()?;
    Ok(Rule {
        process: raw.process,
        class: raw.class,
        title,
        action: raw.action,
        workspace: raw.workspace,
    })
}

#[cfg(test)]
mod tests {
    use rivewm_core::{Direction, MonitorId, Rect, WindowId};

    use super::*;

    fn window(process: &str, class: &str, title: &str) -> WindowInfo {
        WindowInfo {
            id: WindowId(1),
            title: title.into(),
            class: class.into(),
            pid: 1,
            process: Some(process.into()),
            frame: Rect::default(),
            window_rect: Rect::default(),
            monitor: MonitorId(1),
            minimized: false,
            skip: None,
            floating: false,
        }
    }

    #[test]
    fn default_config_parses() {
        let config = parse(DEFAULT_CONFIG).unwrap();
        assert_eq!(config.gaps, Gaps { inner: 8, outer: 8 });
        assert!(config.rules.is_empty());
        assert!(!config.floating_on_top);
        assert_eq!(config.default_layout, Layout::Dwindle);
        let focus_left = "alt+h".parse::<Hotkey>().unwrap();
        assert!(
            config
                .bindings
                .contains(&(focus_left, Command::Focus(Direction::Left)))
        );
        assert_eq!(config.bindings.len(), 48);
    }

    #[test]
    fn missing_sections_fall_back_to_defaults() {
        let config = parse("[gaps]\ninner = 2\n").unwrap();
        assert_eq!(config.gaps, Gaps { inner: 2, outer: 8 });
        assert_eq!(config.bindings.len(), 48);
    }

    #[test]
    fn border_colours() {
        let defaults = parse(DEFAULT_CONFIG).unwrap().border;
        assert!(defaults.enabled);
        assert_eq!(defaults.focused, BorderColor::Rgb(0x33, 0xcc, 0xff));
        assert_eq!(defaults.unfocused, BorderColor::Rgb(0x59, 0x59, 0x59));

        let custom = parse("[border]\nfocused = \"#FF0080\"\nunfocused = \"default\"")
            .unwrap()
            .border;
        assert_eq!(custom.focused, BorderColor::Rgb(0xff, 0x00, 0x80));
        assert_eq!(custom.unfocused, BorderColor::Default);
        assert_eq!(
            parse("[border]\nunfocused = \"none\"")
                .unwrap()
                .border
                .unfocused,
            BorderColor::Hidden
        );
        assert!(!parse("[border]\nenabled = false").unwrap().border.enabled);

        for bad in ["#12345", "#gggggg", "red", "33ccff"] {
            let text = format!("[border]\nfocused = \"{bad}\"");
            let err = format!("{:#}", parse(&text).unwrap_err());
            assert!(err.contains("focused") && err.contains(bad), "{err}");
        }
    }

    #[test]
    fn focus_follows_mouse_option() {
        assert!(!parse(DEFAULT_CONFIG).unwrap().focus_follows_mouse);
        let on = parse("[focus]\nfollows_mouse = true").unwrap();
        assert!(on.focus_follows_mouse);
    }

    #[test]
    fn live_resize_option() {
        assert!(parse(DEFAULT_CONFIG).unwrap().live_resize);
        assert!(!parse("[resize]\nlive = false").unwrap().live_resize);
    }

    #[test]
    fn layout_option() {
        let manual = parse("[layout]\ndefault = \"manual\"").unwrap();
        assert_eq!(manual.default_layout, Layout::Manual);
        let err = format!("{:#}", parse("[layout]\ndefault = \"spiral\"").unwrap_err());
        assert!(err.contains("spiral"));
    }

    #[test]
    fn floating_on_top_option() {
        assert!(parse("[floating]\non_top = true").unwrap().floating_on_top);
        assert!(!parse("[floating]\n").unwrap().floating_on_top);
    }

    #[test]
    fn keybindings_section_replaces_defaults() {
        let config = parse("[keybindings]\n\"win+q\" = \"quit\"\n").unwrap();
        assert_eq!(
            config.bindings,
            vec![("win+q".parse().unwrap(), Command::Quit)]
        );
    }

    #[test]
    fn errors_name_the_problem() {
        let err = |text: &str| format!("{:#}", parse(text).unwrap_err());
        assert!(err("[keybindings]\n\"hyper+x\" = \"quit\"").contains("hyper"));
        assert!(err("[keybindings]\n\"alt+x\" = \"explode\"").contains("alt+x"));
        assert!(err("[gapz]\ninner = 1").contains("gapz"));
        assert!(err("[[rules]]\naction = \"float\"").contains("rule #1"));
        assert!(err("[[rules]]\nprocess = \"a.exe\"").contains("action"));
        assert!(err("[[rules]]\ntitle = \"(\"\naction = \"float\"").contains("regex"));
        assert!(err("[[rules]]\nprocess = \"a.exe\"\naction = \"explode\"").contains("explode"));
        assert!(err("[gaps]\ninner = -1").contains("negative"));
    }

    #[test]
    fn first_matching_rule_wins() {
        let config = parse(
            r#"
            [[rules]]
            process = "spotify.exe"
            workspace = "9"

            [[rules]]
            title = "^Picture-in-picture$"
            action = "float"

            [[rules]]
            class = "CabinetWClass"
            title = "Downloads"
            action = "ignore"

            [[rules]]
            process = "Spotify.exe"
            action = "float"
            "#,
        )
        .unwrap();

        let rule = config.rule_for(&window("Spotify.exe", "Chrome_WidgetWin_1", "Spotify"));
        assert_eq!(rule.unwrap().workspace.as_deref(), Some("9"));
        assert_eq!(rule.unwrap().action, None);

        let pip = window("brave.exe", "Chrome_WidgetWin_1", "Picture-in-picture");
        assert_eq!(
            config.rule_for(&pip).unwrap().action,
            Some(RuleAction::Float)
        );
        let not_pip = window("brave.exe", "Chrome_WidgetWin_1", "Picture-in-picture help");
        assert!(config.rule_for(&not_pip).is_none());

        // Every matcher in a rule must match.
        let downloads = window("explorer.exe", "CabinetWClass", "Downloads - File Explorer");
        assert_eq!(
            config.rule_for(&downloads).unwrap().action,
            Some(RuleAction::Ignore)
        );
        let documents = window("explorer.exe", "CabinetWClass", "Documents - File Explorer");
        assert!(config.rule_for(&documents).is_none());
    }
}
