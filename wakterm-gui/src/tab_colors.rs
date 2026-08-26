use crate::termwindow::TabInformation;
#[cfg(test)]
use config::TabBarColors;
use config::{
    ConfigHandle, RgbaColor, SrgbaTuple, TabBarColorIntensity, TabBarColorMode, TabBarColorPalette,
    CACHE_DIR,
};
use lazy_static::lazy_static;
use mux::pane::CachePolicy;
use mux::Mux;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::convert::TryInto;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const ASSIGNMENT_CACHE_VERSION: u8 = 2;
lazy_static! {
    static ref ASSIGNMENT_STORE: Mutex<AssignmentStore> = Mutex::new(AssignmentStore::default());
}
static DARK_PALETTE: LazyLock<Vec<RgbaColor>> =
    LazyLock::new(|| parse_generated_palette(crate::tab_color_palette::DARK));
static LIGHT_PALETTE: LazyLock<Vec<RgbaColor>> =
    LazyLock::new(|| parse_generated_palette(crate::tab_color_palette::LIGHT));
static MIXED_PALETTE: LazyLock<Vec<RgbaColor>> =
    LazyLock::new(|| parse_generated_palette(crate::tab_color_palette::MIXED));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabColorVisualState {
    Active,
    Hover,
    Inactive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TabRenderColors {
    pub bg: RgbaColor,
    pub fg: RgbaColor,
}

#[derive(Debug, Default)]
struct AssignmentStore {
    loaded: bool,
    assignments: BTreeMap<String, RgbaColor>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedAssignments {
    version: u8,
    assignments: BTreeMap<String, String>,
}

pub fn assign_tab_colors(config: &ConfigHandle, tabs: &mut [TabInformation]) {
    if config.tab_bar_color_mode == TabBarColorMode::Off {
        for tab in tabs.iter_mut() {
            tab.assigned_color.take();
        }
        return;
    }

    let keys_by_tab: Vec<(usize, String)> = tabs
        .iter()
        .enumerate()
        .map(|(idx, tab)| (idx, stable_tab_key(tab)))
        .collect();

    let unique_keys: BTreeSet<String> = keys_by_tab.iter().map(|(_, key)| key.clone()).collect();
    let palette = candidate_palette(config.tab_bar_color_palette);

    let colors_by_key = match config.tab_bar_color_mode {
        TabBarColorMode::Off => HashMap::new(),
        TabBarColorMode::Hash => unique_keys
            .into_iter()
            .map(|key| {
                let preferred_idx = preferred_candidate_idx(&key, palette.len());
                (key, palette[preferred_idx])
            })
            .collect(),
        TabBarColorMode::Assign => assigned_colors_for_keys(unique_keys, palette),
    };

    for (idx, key) in keys_by_tab {
        tabs[idx].assigned_color = colors_by_key.get(&key).copied();
    }
}

#[cfg(test)]
fn tab_bar_background(config: &ConfigHandle) -> RgbaColor {
    config
        .resolved_palette
        .tab_bar
        .as_ref()
        .map(TabBarColors::background)
        .unwrap_or_else(|| TabBarColors::default().background())
}

pub fn tab_render_colors(
    base: RgbaColor,
    _bar_background: RgbaColor,
    state: TabColorVisualState,
    intensity: &TabBarColorIntensity,
) -> TabRenderColors {
    let bg = match state {
        TabColorVisualState::Active => dim_srgba(base, intensity.active),
        TabColorVisualState::Hover => dim_srgba(base, intensity.hover),
        TabColorVisualState::Inactive => dim_srgba(base, intensity.inactive),
    };

    let fg = match state {
        TabColorVisualState::Active => active_text(),
        TabColorVisualState::Hover => hover_text(),
        TabColorVisualState::Inactive => inactive_text(),
    };

    TabRenderColors { bg, fg }
}

fn stable_tab_key(tab: &TabInformation) -> String {
    let effective = tab.effective_title();
    if !effective.is_empty() {
        return named_tab_key(&effective);
    }

    if let Some(cwd) = active_pane_cwd(tab) {
        return named_tab_key(&cwd);
    }

    format!("tab-id:{}", tab.tab_id)
}

fn named_tab_key(name: &str) -> String {
    format!("name:{name}")
}

fn active_pane_cwd(tab: &TabInformation) -> Option<String> {
    let pane_id = tab.active_pane.as_ref()?.pane_id;
    let mux = Mux::try_get()?;
    let pane = mux.get_pane(pane_id)?;
    pane.get_current_working_dir(CachePolicy::AllowStale)
        .map(|url| cwd_key_from_url(&url))
}

fn cwd_key_from_url(url: &url::Url) -> String {
    url.path_segments()
        .and_then(|segments| {
            segments
                .filter(|segment| !segment.is_empty())
                .next_back()
                .map(str::to_string)
        })
        .or_else(|| {
            Path::new(url.path())
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| url.to_string())
}

fn assigned_colors_for_keys(
    keys: BTreeSet<String>,
    palette: &[RgbaColor],
) -> HashMap<String, RgbaColor> {
    let cache_path = assignment_cache_path();
    let mut store = ASSIGNMENT_STORE.lock();
    store.ensure_loaded(&cache_path);

    let dirty_before = store.assignments.len();
    let result = assign_colors_for_keys(&mut store.assignments, keys, palette);
    let dirty = store.assignments.len() != dirty_before;

    if dirty {
        if let Err(err) = store.save_to(&cache_path) {
            log::warn!(
                "failed to persist tab color assignments to {}: {err:#}",
                cache_path.display()
            );
        }
    }

    result
}

fn assign_colors_for_keys(
    assignments: &mut BTreeMap<String, RgbaColor>,
    keys: BTreeSet<String>,
    palette: &[RgbaColor],
) -> HashMap<String, RgbaColor> {
    let mut result = HashMap::new();

    for key in keys {
        let color = if let Some(color) = assignments.get(&key).copied() {
            color
        } else {
            let color = choose_next_assigned_color(&key, assignments.values().copied(), palette);
            assignments.insert(key.clone(), color);
            color
        };
        result.insert(key, color);
    }

    result
}

fn choose_next_assigned_color(
    key: &str,
    existing: impl IntoIterator<Item = RgbaColor>,
    palette: &[RgbaColor],
) -> RgbaColor {
    let used: HashSet<RgbaColor> = existing.into_iter().collect();
    palette
        .iter()
        .copied()
        .find(|color| !used.contains(color))
        .unwrap_or_else(|| palette[preferred_candidate_idx(key, palette.len())])
}

fn preferred_candidate_idx(key: &str, len: usize) -> usize {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    (hasher.finish() as usize) % len
}

fn candidate_palette(kind: TabBarColorPalette) -> &'static [RgbaColor] {
    match kind {
        TabBarColorPalette::Dark => &DARK_PALETTE,
        TabBarColorPalette::Light => &LIGHT_PALETTE,
        TabBarColorPalette::Mixed => &MIXED_PALETTE,
    }
}

fn parse_generated_palette(colors: &[&str]) -> Vec<RgbaColor> {
    colors.iter().copied().map(hex_color).collect()
}

fn hex_color(hex: &str) -> RgbaColor {
    let hex = hex.strip_prefix('#').expect("valid #RRGGBB color");
    let r = u8::from_str_radix(&hex[0..2], 16).expect("valid hex red");
    let g = u8::from_str_radix(&hex[2..4], 16).expect("valid hex green");
    let b = u8::from_str_radix(&hex[4..6], 16).expect("valid hex blue");
    RgbaColor::from((r, g, b))
}

#[cfg(test)]
fn inactive_rendered_bg(base: RgbaColor) -> RgbaColor {
    dim_srgba(base, 0.4)
}

fn dim_srgba(color: RgbaColor, factor: f32) -> RgbaColor {
    let factor = factor.clamp(0.0, 1.0);
    let SrgbaTuple(r, g, b, a) = *color;
    RgbaColor::from(SrgbaTuple(r * factor, g * factor, b * factor, a))
}

fn assignment_cache_path() -> PathBuf {
    CACHE_DIR.join("tab-bar-color-assignments-v2.json")
}

fn inactive_text() -> RgbaColor {
    RgbaColor::from(SrgbaTuple(0.5019608, 0.5019608, 0.5019608, 1.0))
}

fn active_text() -> RgbaColor {
    RgbaColor::from(SrgbaTuple(0.11764706, 0.11764706, 0.18039216, 1.0))
}

fn hover_text() -> RgbaColor {
    RgbaColor::from(SrgbaTuple(0.5647059, 0.5647059, 0.5647059, 1.0))
}

impl AssignmentStore {
    fn ensure_loaded(&mut self, path: &Path) {
        if self.loaded {
            return;
        }
        *self = Self::load_from(path);
    }

    fn load_from(path: &Path) -> Self {
        let json = match fs::read_to_string(path) {
            Ok(json) => json,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Self {
                    loaded: true,
                    assignments: BTreeMap::new(),
                }
            }
            Err(err) => {
                log::warn!(
                    "failed to read tab color assignment cache {}: {err:#}",
                    path.display()
                );
                return Self {
                    loaded: true,
                    assignments: BTreeMap::new(),
                };
            }
        };

        let persisted: PersistedAssignments = match serde_json::from_str(&json) {
            Ok(persisted) => persisted,
            Err(err) => {
                log::warn!(
                    "failed to parse tab color assignment cache {}: {err:#}",
                    path.display()
                );
                return Self {
                    loaded: true,
                    assignments: BTreeMap::new(),
                };
            }
        };

        if persisted.version != ASSIGNMENT_CACHE_VERSION {
            return Self {
                loaded: true,
                assignments: BTreeMap::new(),
            };
        }

        let assignments = persisted
            .assignments
            .into_iter()
            .filter_map(|(key, value)| match value.clone().try_into() {
                Ok(color) => Some((key, color)),
                Err(err) => {
                    log::warn!("failed to parse cached tab color {value}: {err:#}");
                    None
                }
            })
            .collect();

        Self {
            loaded: true,
            assignments,
        }
    }

    fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let persisted = PersistedAssignments {
            version: ASSIGNMENT_CACHE_VERSION,
            assignments: self
                .assignments
                .iter()
                .map(|(key, color)| (key.clone(), String::from(*color)))
                .collect(),
        };
        fs::write(path, serde_json::to_string_pretty(&persisted)? + "\n")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        active_text, assign_colors_for_keys, candidate_palette, choose_next_assigned_color,
        cwd_key_from_url, dim_srgba, hex_color, hover_text, inactive_rendered_bg, inactive_text,
        named_tab_key, stable_tab_key, tab_bar_background, tab_render_colors, AssignmentStore,
    };
    use crate::termwindow::{PaneInformation, TabInformation};
    use config::{ConfigHandle, RgbaColor, TabBarColorIntensity, TabBarColorPalette};
    use std::collections::{BTreeMap, BTreeSet, HashSet};
    use tempfile::tempdir;
    use wakterm_term::Progress;

    fn tab(tab_id: usize, title: &str) -> TabInformation {
        TabInformation {
            tab_id: tab_id as _,
            tab_index: 0,
            is_active: false,
            is_last_active: false,
            active_pane: Some(PaneInformation {
                pane_id: tab_id as _,
                pane_index: 0,
                is_active: true,
                is_zoomed: false,
                has_unseen_output: false,
                left: 0,
                top: 0,
                width: 80,
                height: 24,
                pixel_width: 800,
                pixel_height: 480,
                title: title.to_string(),
                user_vars: Default::default(),
                progress: Progress::None,
            }),
            harness_icons: vec![],
            needs_attention: false,
            assigned_color: None,
            window_id: 0,
            effective_title: title.to_string(),
            tab_title: title.to_string(),
        }
    }

    #[test]
    fn stable_tab_key_prefers_explicit_tab_title() {
        let tab = tab(1, "debate");
        assert_eq!(stable_tab_key(&tab), "name:debate");
    }

    #[test]
    fn stable_tab_key_ignores_decorated_display_title() {
        let mut tab = tab(1, "application");
        tab.tab_title = "⠹ application".to_string();
        assert_eq!(stable_tab_key(&tab), "name:application");
    }

    #[test]
    fn stable_tab_key_respects_collision_suffix() {
        let first = tab(1, "zsh");
        let second = tab(2, "zsh2");
        let third = tab(3, "zsh3");
        assert_eq!(stable_tab_key(&first), "name:zsh");
        assert_eq!(stable_tab_key(&second), "name:zsh2");
        assert_eq!(stable_tab_key(&third), "name:zsh3");
    }

    #[test]
    fn title_and_cwd_names_share_key_namespace() {
        let cwd = url::Url::parse("file:///code/x").unwrap();
        assert_eq!(named_tab_key("x"), named_tab_key(&cwd_key_from_url(&cwd)));
    }

    #[test]
    fn cwd_key_from_url_uses_last_unix_segment() {
        let url = url::Url::parse("file://fedora/code/wakterm").unwrap();
        assert_eq!(cwd_key_from_url(&url), "wakterm");
    }

    #[test]
    fn cwd_key_from_url_handles_windows_file_url() {
        let url = url::Url::parse("file:///C:/Users/Mihai/code/wakterm").unwrap();
        assert_eq!(cwd_key_from_url(&url), "wakterm");
    }

    #[test]
    fn load_and_save_assignment_store_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tab-colors.json");
        let mut store = AssignmentStore {
            loaded: true,
            assignments: BTreeMap::new(),
        };
        store
            .assignments
            .insert("title:one".to_string(), hex_color("#2885ef"));
        store.save_to(&path).unwrap();

        let loaded = AssignmentStore::load_from(&path);
        assert_eq!(
            loaded
                .assignments
                .into_iter()
                .map(|(key, color)| (key, String::from(color)))
                .collect::<BTreeMap<_, _>>(),
            store
                .assignments
                .into_iter()
                .map(|(key, color)| (key, String::from(color)))
                .collect::<BTreeMap<_, _>>()
        );
    }

    #[test]
    fn old_assignment_cache_version_is_ignored() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tab-colors.json");
        std::fs::write(
            &path,
            r##"{"version":1,"assignments":{"name:x":"#2885ef"}}"##,
        )
        .unwrap();

        let loaded = AssignmentStore::load_from(&path);
        assert!(loaded.loaded);
        assert!(loaded.assignments.is_empty());
    }

    #[test]
    fn assign_tab_colors_reuses_persisted_title_mapping_after_reload() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tab-colors.json");
        let palette = candidate_palette(TabBarColorPalette::Mixed);
        let key = stable_tab_key(&tab(1, "wakterm"));
        let mut store = AssignmentStore {
            loaded: true,
            assignments: BTreeMap::new(),
        };
        let first_color = assign_colors_for_keys(
            &mut store.assignments,
            BTreeSet::from([key.clone()]),
            palette,
        )[&key];

        store.save_to(&path).unwrap();
        let mut loaded = AssignmentStore::load_from(&path);

        let reloaded_key = stable_tab_key(&tab(2, "wakterm"));
        let second_color = assign_colors_for_keys(
            &mut loaded.assignments,
            BTreeSet::from([reloaded_key.clone()]),
            palette,
        )[&reloaded_key];

        assert_eq!(String::from(second_color), String::from(first_color));
    }

    #[test]
    fn assign_mode_consumes_the_precomputed_sequence_without_reuse() {
        let palette = candidate_palette(TabBarColorPalette::Mixed);
        let existing = palette[..37].iter().copied();
        assert_eq!(
            choose_next_assigned_color("fresh", existing, palette),
            palette[37]
        );
    }

    #[test]
    fn assign_mode_assigns_unseen_keys_independent_of_input_order() {
        let mut first = BTreeMap::from([("existing".to_string(), hex_color("#2885ef"))]);
        let mut second = first.clone();
        let palette = candidate_palette(TabBarColorPalette::Mixed);

        let first_result = assign_colors_for_keys(
            &mut first,
            Vec::from(["bravo".to_string(), "alpha".to_string()])
                .into_iter()
                .collect(),
            palette,
        );
        let second_result = assign_colors_for_keys(
            &mut second,
            Vec::from(["alpha".to_string(), "bravo".to_string()])
                .into_iter()
                .collect(),
            palette,
        );

        assert_eq!(first_result, second_result);
        assert_eq!(first, second);
    }

    #[test]
    fn generated_palettes_are_full_and_have_no_duplicate_display_colors() {
        for kind in [
            TabBarColorPalette::Dark,
            TabBarColorPalette::Light,
            TabBarColorPalette::Mixed,
        ] {
            let palette = candidate_palette(kind);
            assert_eq!(palette.len(), 512);
            assert_eq!(palette.iter().copied().collect::<HashSet<_>>().len(), 512);
            assert_eq!(
                palette
                    .iter()
                    .map(|color| {
                        let config::SrgbaTuple(r, g, b, _) = **color;
                        (
                            (r * 0.4 * 255.0).round() as u8,
                            (g * 0.4 * 255.0).round() as u8,
                            (b * 0.4 * 255.0).round() as u8,
                        )
                    })
                    .collect::<HashSet<_>>()
                    .len(),
                512
            );
        }
    }

    #[test]
    fn generated_palettes_have_independent_prefixes() {
        assert_ne!(
            &candidate_palette(TabBarColorPalette::Dark)[..24],
            &candidate_palette(TabBarColorPalette::Mixed)[..24]
        );
    }

    #[test]
    fn active_tab_render_colors_use_fixed_lua_foreground() {
        let rendered = tab_render_colors(
            RgbaColor::from((40, 133, 239)),
            tab_bar_background(&ConfigHandle::default_config()),
            super::TabColorVisualState::Active,
            &ConfigHandle::default_config().tab_bar_color_intensity,
        );
        assert_eq!(rendered.fg, active_text());
    }

    #[test]
    fn inactive_tab_render_colors_use_fixed_lua_foreground() {
        let rendered = tab_render_colors(
            RgbaColor::from((255, 146, 126)),
            tab_bar_background(&ConfigHandle::default_config()),
            super::TabColorVisualState::Inactive,
            &ConfigHandle::default_config().tab_bar_color_intensity,
        );
        assert_eq!(rendered.fg, inactive_text());
        assert_eq!(
            rendered.bg,
            inactive_rendered_bg(RgbaColor::from((255, 146, 126)))
        );
    }

    #[test]
    fn hover_tab_render_colors_use_fixed_lua_foreground() {
        let rendered = tab_render_colors(
            RgbaColor::from((40, 133, 239)),
            tab_bar_background(&ConfigHandle::default_config()),
            super::TabColorVisualState::Hover,
            &ConfigHandle::default_config().tab_bar_color_intensity,
        );
        assert_eq!(rendered.fg, hover_text());
    }

    #[test]
    fn tab_render_colors_respect_configured_intensity() {
        let config = ConfigHandle::default_config();
        let intensity = TabBarColorIntensity {
            active: 0.9,
            hover: 0.7,
            inactive: 0.5,
        };

        assert_eq!(
            tab_render_colors(
                RgbaColor::from((40, 133, 239)),
                tab_bar_background(&config),
                super::TabColorVisualState::Active,
                &intensity,
            )
            .bg,
            dim_srgba(RgbaColor::from((40, 133, 239)), 0.9)
        );
        assert_eq!(
            tab_render_colors(
                RgbaColor::from((40, 133, 239)),
                tab_bar_background(&config),
                super::TabColorVisualState::Hover,
                &intensity,
            )
            .bg,
            dim_srgba(RgbaColor::from((40, 133, 239)), 0.7)
        );
        assert_eq!(
            tab_render_colors(
                RgbaColor::from((40, 133, 239)),
                tab_bar_background(&config),
                super::TabColorVisualState::Inactive,
                &intensity,
            )
            .bg,
            dim_srgba(RgbaColor::from((40, 133, 239)), 0.5)
        );
    }
}
