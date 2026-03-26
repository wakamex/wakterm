use crate::termwindow::TabInformation;
use config::{
    ConfigHandle, RgbaColor, SrgbaTuple, TabBarColorMode, TabBarColorPalette, CACHE_DIR,
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
    palette: &'static [RgbaColor],
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
            let color = choose_most_distinct_color(&key, assignments.values().copied(), palette);
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
            let score_a = min_color_distance(*color_a, &existing);
            let score_b = min_color_distance(*color_b, &existing);
            score_a
                .partial_cmp(&score_b)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    circular_distance(*idx_b, preferred_idx, palette.len())
                        .cmp(&circular_distance(*idx_a, preferred_idx, palette.len()))
                })
        })
        .map(|(_, color)| color)
        .unwrap_or(palette[preferred_idx])
}

fn min_color_distance(candidate: RgbaColor, existing: &[RgbaColor]) -> f32 {
    if existing.is_empty() {
        return f32::MAX;
    }

    existing
        .iter()
        .copied()
        .map(|existing| color_distance(candidate, existing))
        .fold(f32::INFINITY, f32::min)
}

fn color_distance(a: RgbaColor, b: RgbaColor) -> f32 {
    let (ar, ag, ab) = linear_rgb(a);
    let (br, bg, bb) = linear_rgb(b);
    let dr = ar - br;
    let dg = ag - bg;
    let db = ab - bb;
    dr * dr + dg * dg + db * db
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

fn candidate_palette(kind: TabBarColorPalette) -> &'static [RgbaColor] {
    lazy_static! {
        static ref MIXED_PALETTE: Vec<RgbaColor> = {
            let rings = [
                (0.76_f32, 0.92_f32, 0.0_f32),
                (0.68_f32, 0.82_f32, 0.5_f32 / 72.0_f32),
                (0.84_f32, 0.74_f32, 0.25_f32 / 72.0_f32),
            ];
            let steps = 72;
            let mut colors = Vec::with_capacity(rings.len() * steps);
            for (saturation, value, offset) in rings {
                for idx in 0..steps {
                    let hue = ((idx as f32) / (steps as f32) + offset).fract();
                    colors.push(hsv_to_rgba(hue, saturation, value));
                }
            }
            colors
        };
        static ref DARK_PALETTE: Vec<RgbaColor> = MIXED_PALETTE
            .iter()
            .copied()
            .filter(|color| prefers_light_text(*color))
            .collect();
        static ref LIGHT_PALETTE: Vec<RgbaColor> = MIXED_PALETTE
            .iter()
            .copied()
            .filter(|color| prefers_dark_text(*color))
            .collect();
    }

    match kind {
        TabBarColorPalette::Dark => DARK_PALETTE.as_slice(),
        TabBarColorPalette::Light => LIGHT_PALETTE.as_slice(),
        TabBarColorPalette::Mixed => MIXED_PALETTE.as_slice(),
    }
}

fn hsv_to_rgba(h: f32, s: f32, v: f32) -> RgbaColor {
    let h = (h.fract() + 1.0).fract() * 6.0;
    let i = h.floor() as i32;
    let f = h - i as f32;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));

    let (r, g, b) = match i.rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };

    RgbaColor::from(SrgbaTuple(r, g, b, 1.0))
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
                log::warn!("failed to read tab color assignment cache {}: {err:#}", path.display());
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
        hsv_to_rgba, prefers_dark_text, prefers_light_text, stable_tab_key, AssignmentStore,
    };
    use crate::termwindow::{PaneInformation, TabInformation};
    use config::TabBarColorPalette;
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
        store
            .assignments
            .insert("title:one".to_string(), hsv_to_rgba(0.25, 0.7, 0.9));
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
            hsv_to_rgba(0.0, 0.76, 0.92),
            hsv_to_rgba(0.33, 0.76, 0.92),
            hsv_to_rgba(0.66, 0.76, 0.92),
        ];
        let palette = candidate_palette(TabBarColorPalette::Mixed);
        let chosen = choose_most_distinct_color("fresh", existing, palette);

        assert_eq!(
            chosen,
            choose_most_distinct_color("fresh", existing, palette)
        );
        assert!(!existing.contains(&chosen));
    }

    #[test]
    fn assign_mode_assigns_unseen_keys_independent_of_input_order() {
        let mut first = BTreeMap::from([("existing".to_string(), hsv_to_rgba(0.0, 0.76, 0.92))]);
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
    fn dark_palette_prefers_light_text() {
        assert!(
            candidate_palette(TabBarColorPalette::Dark)
                .iter()
                .copied()
                .all(prefers_light_text)
        );
    }

    #[test]
    fn light_palette_prefers_dark_text() {
        assert!(
            candidate_palette(TabBarColorPalette::Light)
                .iter()
                .copied()
                .all(prefers_dark_text)
        );
    }

    #[test]
    fn hsv_to_rgba_produces_opaque_color() {
        let color = hsv_to_rgba(0.5, 0.7, 0.8);
        let config::SrgbaTuple(_, _, _, alpha) = *color;
        assert_eq!(alpha, 1.0);
    }
}
