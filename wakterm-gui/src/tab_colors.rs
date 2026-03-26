use crate::termwindow::TabInformation;
use config::{
    ConfigHandle, RgbaColor, SrgbaTuple, TabBarColorMode, TabBarColorPalette, TabBarColors,
    CACHE_DIR,
};
use lazy_static::lazy_static;
use mux::pane::CachePolicy;
use mux::Mux;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::convert::TryInto;
use std::f32::consts::TAU;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

const ASSIGNMENT_CACHE_VERSION: u8 = 1;
lazy_static! {
    static ref ASSIGNMENT_STORE: Mutex<AssignmentStore> = Mutex::new(AssignmentStore::default());
}

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
    let bar_background = tab_bar_background(config);
    let palette = candidate_palette(config.tab_bar_color_palette, bar_background);

    let colors_by_key = match config.tab_bar_color_mode {
        TabBarColorMode::Off => HashMap::new(),
        TabBarColorMode::Hash => unique_keys
            .into_iter()
            .map(|key| {
                let preferred_idx = preferred_candidate_idx(&key, palette.len());
                (key, palette[preferred_idx])
            })
            .collect(),
        TabBarColorMode::Assign => assigned_colors_for_keys(unique_keys, &palette, bar_background),
    };

    for (idx, key) in keys_by_tab {
        tabs[idx].assigned_color = colors_by_key.get(&key).copied();
    }
}

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
    bar_background: RgbaColor,
    state: TabColorVisualState,
) -> TabRenderColors {
    let bg = match state {
        TabColorVisualState::Active => base,
        TabColorVisualState::Hover => mix_srgba(base, bar_background, 0.12),
        TabColorVisualState::Inactive => mix_srgba(base, bar_background, 0.24),
    };

    let dark_text = dark_text();
    let light_text = light_text();
    let fg = if contrast_ratio(bg, dark_text) >= contrast_ratio(bg, light_text) {
        dark_text
    } else {
        light_text
    };

    TabRenderColors { bg, fg }
}

fn stable_tab_key(tab: &TabInformation) -> String {
    if !tab.tab_title.is_empty() {
        return format!("tab-title:{}", tab.tab_title);
    }

    if let Some(cwd) = active_pane_cwd(tab) {
        return format!("cwd:{cwd}");
    }

    let effective = tab.effective_title();
    if !effective.is_empty() {
        return format!("title:{effective}");
    }

    format!("tab-id:{}", tab.tab_id)
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
    bar_background: RgbaColor,
) -> HashMap<String, RgbaColor> {
    let cache_path = assignment_cache_path();
    let mut store = ASSIGNMENT_STORE.lock();
    store.ensure_loaded(&cache_path);

    let dirty_before = store.assignments.len();
    let result = assign_colors_for_keys(&mut store.assignments, keys, palette, bar_background);
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
    bar_background: RgbaColor,
) -> HashMap<String, RgbaColor> {
    let mut result = HashMap::new();

    for key in keys {
        let color = if let Some(color) = assignments.get(&key).copied() {
            color
        } else {
            let color = choose_most_distinct_color(
                &key,
                assignments.values().copied(),
                palette,
                bar_background,
            );
            assignments.insert(key.clone(), color);
            color
        };
        result.insert(key, color);
    }

    result
}

fn choose_most_distinct_color(
    key: &str,
    existing: impl IntoIterator<Item = RgbaColor>,
    palette: &[RgbaColor],
    bar_background: RgbaColor,
) -> RgbaColor {
    let existing: Vec<RgbaColor> = existing.into_iter().collect();
    let preferred_idx = preferred_candidate_idx(key, palette.len());
    let used: HashSet<RgbaColor> = existing.iter().copied().collect();

    let candidates: Vec<(usize, RgbaColor)> = palette
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, color)| !used.contains(color))
        .collect();

    let candidates = if candidates.is_empty() {
        palette.iter().copied().enumerate().collect::<Vec<_>>()
    } else {
        candidates
    };

    candidates
        .into_iter()
        .max_by(|(idx_a, color_a), (idx_b, color_b)| {
            let score_a = min_color_distance(*color_a, &existing, bar_background);
            let score_b = min_color_distance(*color_b, &existing, bar_background);
            score_a
                .partial_cmp(&score_b)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    circular_distance(*idx_b, preferred_idx, palette.len()).cmp(&circular_distance(
                        *idx_a,
                        preferred_idx,
                        palette.len(),
                    ))
                })
        })
        .map(|(_, color)| color)
        .unwrap_or(palette[preferred_idx])
}

fn min_color_distance(
    candidate: RgbaColor,
    existing: &[RgbaColor],
    bar_background: RgbaColor,
) -> f32 {
    if existing.is_empty() {
        return f32::MAX;
    }

    let candidate = inactive_rendered_bg(candidate, bar_background);
    existing
        .iter()
        .copied()
        .map(|existing| color_distance(candidate, inactive_rendered_bg(existing, bar_background)))
        .fold(f32::INFINITY, f32::min)
}

fn color_distance(a: RgbaColor, b: RgbaColor) -> f32 {
    let (al, aa, ab) = oklab(a);
    let (bl, ba, bb) = oklab(b);
    let dl = al - bl;
    let da = aa - ba;
    let db = ab - bb;
    dl * dl + da * da + db * db
}

fn preferred_candidate_idx(key: &str, len: usize) -> usize {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    (hasher.finish() as usize) % len
}

fn circular_distance(a: usize, b: usize, len: usize) -> usize {
    let forward = a.abs_diff(b);
    let wrap = len.saturating_sub(forward);
    forward.min(wrap)
}

fn candidate_palette(kind: TabBarColorPalette, bar_background: RgbaColor) -> Vec<RgbaColor> {
    let mut colors = match kind {
        TabBarColorPalette::Dark => curated_dark_palette(),
        TabBarColorPalette::Light => build_oklch_slice(
            &[0.80, 0.84, 0.88],
            &[0.09, 0.12, 0.15],
            &[0.0, 0.5 / 24.0, 1.0 / 24.0],
            24,
        ),
        TabBarColorPalette::Mixed => {
            let mut colors = curated_dark_palette();
            colors.extend(build_oklch_slice(
                &[0.80, 0.84, 0.88],
                &[0.09, 0.12, 0.15],
                &[0.25 / 24.0, 0.75 / 24.0, 1.25 / 24.0],
                24,
            ));
            colors
        }
    };

    colors.retain(|color| match kind {
        TabBarColorPalette::Dark => {
            prefers_light_text(inactive_rendered_bg(*color, bar_background))
        }
        TabBarColorPalette::Light => {
            prefers_dark_text(inactive_rendered_bg(*color, bar_background))
        }
        TabBarColorPalette::Mixed => true,
    });

    colors
}

fn curated_dark_palette() -> Vec<RgbaColor> {
    [
        "#8f3b4d", "#a24d2f", "#9c6400", "#8a7600", "#5d8a00", "#007f4f", "#007c7f", "#006fa8",
        "#2d63c7", "#6b5bd6", "#8f53b0", "#a5477f", "#b34767", "#b35a2c", "#b17a00", "#7f8700",
        "#3c8a16", "#00886f", "#00839d", "#0078c0", "#4d69d6", "#7a63c8", "#9b57b7", "#b24f98",
        "#7c4758", "#8a5a3a", "#7c6708", "#56752f", "#22705b", "#256b79", "#3b5e96", "#6a4f95",
    ]
    .iter()
    .copied()
    .map(hex_color)
    .collect()
}

fn build_oklch_slice(
    lightnesses: &[f32],
    chromas: &[f32],
    offsets: &[f32],
    steps: usize,
) -> Vec<RgbaColor> {
    let mut colors = Vec::with_capacity(lightnesses.len() * chromas.len() * steps);
    for (row, &lightness) in lightnesses.iter().enumerate() {
        for (band, &chroma) in chromas.iter().enumerate() {
            let offset = offsets[(row + band) % offsets.len()];
            for idx in 0..steps {
                let hue = TAU * (((idx as f32) / (steps as f32)) + offset).fract();
                if let Some(color) = oklch_to_rgba(lightness, chroma, hue) {
                    colors.push(color);
                }
            }
        }
    }
    colors
}

fn hex_color(hex: &str) -> RgbaColor {
    let hex = hex.strip_prefix('#').expect("valid #RRGGBB color");
    let r = u8::from_str_radix(&hex[0..2], 16).expect("valid hex red");
    let g = u8::from_str_radix(&hex[2..4], 16).expect("valid hex green");
    let b = u8::from_str_radix(&hex[4..6], 16).expect("valid hex blue");
    RgbaColor::from((r, g, b))
}

fn inactive_rendered_bg(base: RgbaColor, bar_background: RgbaColor) -> RgbaColor {
    mix_srgba(base, bar_background, 0.24)
}

fn oklch_to_rgba(lightness: f32, chroma: f32, hue: f32) -> Option<RgbaColor> {
    let a = chroma * hue.cos();
    let b = chroma * hue.sin();

    let l = lightness + 0.396_337_78 * a + 0.215_803_76 * b;
    let m = lightness - 0.105_561_346 * a - 0.063_854_17 * b;
    let s = lightness - 0.089_484_18 * a - 1.291_485_5 * b;

    let l = l * l * l;
    let m = m * m * m;
    let s = s * s * s;

    let r = 4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s;
    let g = -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s;
    let b = -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s;

    if !(0.0..=1.0).contains(&r) || !(0.0..=1.0).contains(&g) || !(0.0..=1.0).contains(&b) {
        return None;
    }

    Some(RgbaColor::from(SrgbaTuple(
        linear_channel_to_srgb(r),
        linear_channel_to_srgb(g),
        linear_channel_to_srgb(b),
        1.0,
    )))
}

fn mix_srgba(a: RgbaColor, b: RgbaColor, amount: f32) -> RgbaColor {
    let amount = amount.clamp(0.0, 1.0);
    let SrgbaTuple(ar, ag, ab, aa) = *a;
    let SrgbaTuple(br, bg, bb, ba) = *b;
    RgbaColor::from(SrgbaTuple(
        ar + (br - ar) * amount,
        ag + (bg - ag) * amount,
        ab + (bb - ab) * amount,
        aa + (ba - aa) * amount,
    ))
}

fn contrast_ratio(a: RgbaColor, b: RgbaColor) -> f32 {
    let l1 = relative_luminance(a);
    let l2 = relative_luminance(b);
    let (lighter, darker) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
    (lighter + 0.05) / (darker + 0.05)
}

fn prefers_dark_text(color: RgbaColor) -> bool {
    contrast_ratio(color, dark_text()) >= contrast_ratio(color, light_text())
}

fn prefers_light_text(color: RgbaColor) -> bool {
    !prefers_dark_text(color)
}

fn relative_luminance(color: RgbaColor) -> f32 {
    let (r, g, b) = linear_rgb(color);
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

fn linear_rgb(color: RgbaColor) -> (f32, f32, f32) {
    let SrgbaTuple(r, g, b, _) = *color;
    (
        srgb_channel_to_linear(r),
        srgb_channel_to_linear(g),
        srgb_channel_to_linear(b),
    )
}

fn srgb_channel_to_linear(value: f32) -> f32 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_channel_to_srgb(value: f32) -> f32 {
    if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

fn oklab(color: RgbaColor) -> (f32, f32, f32) {
    let (r, g, b) = linear_rgb(color);

    let l = (0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();

    (
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    )
}

fn assignment_cache_path() -> PathBuf {
    CACHE_DIR.join("tab-bar-color-assignments-v1.json")
}

fn dark_text() -> RgbaColor {
    RgbaColor::from(SrgbaTuple(0.11764706, 0.11764706, 0.18039216, 1.0))
}

fn light_text() -> RgbaColor {
    RgbaColor::from(SrgbaTuple(0.8666667, 0.8666667, 0.8666667, 1.0))
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
        assign_colors_for_keys, candidate_palette, choose_most_distinct_color, cwd_key_from_url,
        inactive_rendered_bg, oklch_to_rgba, prefers_dark_text, prefers_light_text, stable_tab_key,
        tab_bar_background, AssignmentStore,
    };
    use crate::termwindow::{PaneInformation, TabInformation};
    use config::{ConfigHandle, TabBarColorPalette};
    use std::collections::BTreeMap;
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
            harness_icon: None,
            assigned_color: None,
            window_id: 0,
            tab_title: title.to_string(),
        }
    }

    #[test]
    fn stable_tab_key_prefers_explicit_tab_title() {
        let tab = tab(1, "debate");
        assert_eq!(stable_tab_key(&tab), "tab-title:debate");
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
        store.assignments.insert(
            "title:one".to_string(),
            oklch_to_rgba(0.62, 0.14, 1.0).unwrap(),
        );
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
    fn choose_most_distinct_color_is_deterministic_and_avoids_reuse_when_possible() {
        let existing = [
            oklch_to_rgba(0.62, 0.14, 0.1).unwrap(),
            oklch_to_rgba(0.62, 0.14, 2.0).unwrap(),
            oklch_to_rgba(0.62, 0.12, 4.1).unwrap(),
        ];
        let background = tab_bar_background(&ConfigHandle::default_config());
        let palette = candidate_palette(TabBarColorPalette::Mixed, background);
        let chosen = choose_most_distinct_color("fresh", existing, &palette, background);

        assert_eq!(
            chosen,
            choose_most_distinct_color("fresh", existing, &palette, background)
        );
        assert!(!existing.contains(&chosen));
    }

    #[test]
    fn assign_mode_assigns_unseen_keys_independent_of_input_order() {
        let mut first = BTreeMap::from([(
            "existing".to_string(),
            oklch_to_rgba(0.62, 0.14, 0.2).unwrap(),
        )]);
        let mut second = first.clone();
        let background = tab_bar_background(&ConfigHandle::default_config());
        let palette = candidate_palette(TabBarColorPalette::Mixed, background);

        let first_result = assign_colors_for_keys(
            &mut first,
            Vec::from(["bravo".to_string(), "alpha".to_string()])
                .into_iter()
                .collect(),
            &palette,
            background,
        );
        let second_result = assign_colors_for_keys(
            &mut second,
            Vec::from(["alpha".to_string(), "bravo".to_string()])
                .into_iter()
                .collect(),
            &palette,
            background,
        );

        assert_eq!(first_result, second_result);
        assert_eq!(first, second);
    }

    #[test]
    fn dark_palette_prefers_light_text() {
        let background = tab_bar_background(&ConfigHandle::default_config());
        assert!(candidate_palette(TabBarColorPalette::Dark, background)
            .iter()
            .copied()
            .all(|color| prefers_light_text(inactive_rendered_bg(color, background))));
    }

    #[test]
    fn light_palette_prefers_dark_text() {
        let background = tab_bar_background(&ConfigHandle::default_config());
        assert!(candidate_palette(TabBarColorPalette::Light, background)
            .iter()
            .copied()
            .all(|color| prefers_dark_text(inactive_rendered_bg(color, background))));
    }

    #[test]
    fn oklch_to_rgba_produces_opaque_color() {
        let color = oklch_to_rgba(0.62, 0.10, 1.4).unwrap();
        let config::SrgbaTuple(_, _, _, alpha) = *color;
        assert_eq!(alpha, 1.0);
    }
}
