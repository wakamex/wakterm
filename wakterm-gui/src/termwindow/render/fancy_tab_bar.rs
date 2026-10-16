use crate::color::LinearRgba;
use crate::customglyph::*;
use crate::quad::QuadTrait;
use crate::tab_colors::{tab_render_colors, TabColorVisualState};
use crate::tabbar::{TabBarItem, TabEntry};
use crate::termwindow::box_model::*;
use crate::termwindow::render::window_buttons::window_button_element;
use crate::termwindow::{TabHarnessIcon, TermWindowNotif, UIItem, UIItemType};
use crate::utilsprites::RenderMetrics;
use config::{Dimension, DimensionContext, TabBarColors};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use termwiz::cell::unicode_column_width;
use termwiz::surface::SEQ_ZERO;
use wakterm_font::LoadedFont;
use wakterm_term::color::{ColorAttribute, ColorPalette};
use wakterm_term::{Line, TerminalConfiguration};
use window::{IntegratedTitleButtonAlignment, IntegratedTitleButtonStyle, WindowOps};

const X_BUTTON: &[Poly] = &[
    Poly {
        path: &[
            PolyCommand::MoveTo(BlockCoord::One, BlockCoord::Zero),
            PolyCommand::LineTo(BlockCoord::Zero, BlockCoord::One),
        ],
        intensity: BlockAlpha::Full,
        style: PolyStyle::Outline,
    },
    Poly {
        path: &[
            PolyCommand::MoveTo(BlockCoord::Zero, BlockCoord::Zero),
            PolyCommand::LineTo(BlockCoord::One, BlockCoord::One),
        ],
        intensity: BlockAlpha::Full,
        style: PolyStyle::Outline,
    },
];

const ATTENTION_DOT: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Zero, BlockCoord::Zero),
        PolyCommand::LineTo(BlockCoord::One, BlockCoord::Zero),
        PolyCommand::LineTo(BlockCoord::One, BlockCoord::One),
        PolyCommand::LineTo(BlockCoord::Zero, BlockCoord::One),
        PolyCommand::Close,
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Fill,
}];

/// How far toward the tab's background a cut title's last characters are
/// drawn: two characters while the title is long, one as it nears its
/// shortest.
const TITLE_FADE_STEPS: [f32; 2] = [0.45, 0.75];
const TITLE_FADE_STEP: f32 = 0.6;
/// How many characters of a cut title stay fully drawn. A tab with a harness
/// icon narrows further by sliding its title under the icon.
const MIN_SHRUNK_TITLE_CHARS: usize = 3;
/// How far a working agent's tab is drawn toward the tab bar's background,
/// and its icon toward transparent; and how long its activity marker takes
/// to blink.
const WORKING_TAB_DIM: f32 = 0.55;
const WORKING_MARKER_PERIOD: Duration = Duration::from_millis(1200);
/// The width of the bar in which tabs are measured: wide enough that no tab
/// is squeezed.
const MEASURE_WIDTH: f32 = 1_000_000.;

/// How a tab's title is drawn when the tabs do not fit: how far each kept
/// character is faded toward the tab's background, by the byte offset where
/// the character ends, and the width of the space left of the title where
/// the harness icon is drawn, when that space is narrowed.
#[derive(Clone, Debug, Default, PartialEq)]
struct TitleCut {
    fades: Vec<(usize, f32)>,
    icon_space: Option<f32>,
}

/// How to draw a title of these character widths, as cumulative widths at
/// each character boundary, in `allowance` pixels. With an icon, the space
/// before the title is `icon_space` wide, and narrowing it slides the title
/// under the icon. Returns how many characters to keep and how to draw them.
fn cut_title(
    prefixes: &[(usize, f32)],
    allowance: f32,
    icon_space: Option<f32>,
) -> (usize, TitleCut) {
    let width_of = |chars: usize| {
        if chars == 0 {
            0.
        } else {
            prefixes[chars - 1].1
        }
    };
    let fitting = |width: f32| prefixes.iter().take_while(|(_, w)| *w <= width).count();
    let min_chars = prefixes.len().min(MIN_SHRUNK_TITLE_CHARS);
    let fit = fitting(allowance);
    if fit >= min_chars {
        // The fade covers the last two characters, then fewer as the title
        // nears its shortest, so that its full characters stay readable.
        let fading = (fit - min_chars).min(TITLE_FADE_STEPS.len());
        let steps: &[f32] = match fading {
            0 => &[],
            1 => &[TITLE_FADE_STEP],
            _ => &TITLE_FADE_STEPS,
        };
        let fades = prefixes[..fit]
            .iter()
            .enumerate()
            .map(|(i, (end, _))| {
                let from_end = fit - 1 - i;
                let fade = if from_end < steps.len() {
                    steps[steps.len() - 1 - from_end]
                } else {
                    0.
                };
                (*end, fade)
            })
            .collect();
        return (
            fit,
            TitleCut {
                fades,
                icon_space: None,
            },
        );
    }
    let Some(icon_space) = icon_space else {
        // Without an icon to slide under, the title keeps its shortest form.
        let fades = prefixes[..min_chars]
            .iter()
            .map(|(end, _)| (*end, 0.))
            .collect();
        return (
            min_chars,
            TitleCut {
                fades,
                icon_space: None,
            },
        );
    };
    // The icon space and the title share the tab's room. The title slides
    // left under the icon, and drops characters only once it has slid all
    // the way, and each character fades by how much of it the icon covers.
    let room = icon_space + allowance.max(0.);
    let keep = if room >= width_of(min_chars) {
        min_chars
    } else {
        fitting(room)
    };
    let space = (room - width_of(keep)).max(0.);
    let fades = prefixes[..keep]
        .iter()
        .enumerate()
        .map(|(i, (end, right))| {
            let left = space + width_of(i);
            let right = space + right;
            let covered = ((icon_space - left) / (right - left).max(1.)).clamp(0., 1.);
            (*end, covered)
        })
        .collect();
    (
        keep,
        TitleCut {
            fades,
            icon_space: Some(space),
        },
    )
}

fn mix(from: LinearRgba, to: LinearRgba, k: f32) -> LinearRgba {
    LinearRgba(
        from.0 + k * (to.0 - from.0),
        from.1 + k * (to.1 - from.1),
        from.2 + k * (to.2 - from.2),
        from.3 + k * (to.3 - from.3),
    )
}

static ATTENTION_PULSE_START: LazyLock<Instant> = LazyLock::new(Instant::now);

const PLUS_BUTTON: &[Poly] = &[
    Poly {
        path: &[
            PolyCommand::MoveTo(BlockCoord::Frac(1, 2), BlockCoord::Zero),
            PolyCommand::LineTo(BlockCoord::Frac(1, 2), BlockCoord::One),
        ],
        intensity: BlockAlpha::Full,
        style: PolyStyle::Outline,
    },
    Poly {
        path: &[
            PolyCommand::MoveTo(BlockCoord::Zero, BlockCoord::Frac(1, 2)),
            PolyCommand::LineTo(BlockCoord::One, BlockCoord::Frac(1, 2)),
        ],
        intensity: BlockAlpha::Full,
        style: PolyStyle::Outline,
    },
];

impl crate::TermWindow {
    /// Whether the tab is drawn dimmed: its agents are all working, or, when
    /// that is enabled, its agent waits with a reply already reviewed.
    fn tab_dimmed(&self, entry: &TabEntry) -> bool {
        (self.config.agent_tab_dim_working && entry.agents_working)
            || (self.config.agent_tab_dim_reviewed && entry.agents_reviewed)
    }

    pub fn invalidate_fancy_tab_bar(&mut self) {
        self.fancy_tab_bar.take();
    }

    pub fn build_fancy_tab_bar(&self, palette: &ColorPalette) -> anyhow::Result<ComputedElement> {
        let items = self.tab_bar.items();
        let width = self.dimensions.pixel_width as f32;
        let no_cuts = HashMap::new();
        if !self.config.tab_titles_shrink_to_fit {
            return self.layout_fancy_tab_bar(palette, items, &no_cuts, false, width);
        }
        // Tabs past the window's edge are laid out with no room and lose
        // their width, so the tabs are measured in a bar wide enough for all
        // of them, and only the chosen layout is drawn at the window's width.
        let measure = |items: &[TabEntry], cuts: &HashMap<usize, TitleCut>, compact: bool| {
            self.layout_fancy_tab_bar(palette, items, cuts, compact, MEASURE_WIDTH)
                .map(|computed| computed.ui_items())
        };
        if self.tab_overflow(&measure(items, &no_cuts, false)?) <= 0. {
            return self.layout_fancy_tab_bar(palette, items, &no_cuts, false, width);
        }
        // Tabs that do not fit first lose the close button of every tab but
        // the active one, then title width.
        let compact = measure(items, &no_cuts, true)?;
        let mut chosen = (items.to_vec(), no_cuts.clone());
        // The shared width comes from an estimate of the space the tabs
        // take; whatever still overflows after layout is measured and taken
        // off the space before the next pass.
        let mut slack = 0.;
        for _ in 0..3 {
            let Some((shrunk, cuts)) = self.shrink_tab_titles(items, &compact, slack)? else {
                break;
            };
            let overflow = self.tab_overflow(&measure(&shrunk, &cuts, true)?);
            chosen = (shrunk, cuts);
            if overflow <= 0. {
                break;
            }
            slack += overflow;
        }
        self.layout_fancy_tab_bar(palette, &chosen.0, &chosen.1, true, width)
    }

    /// How far, in pixels, the tabs and the new tab button of a bar laid out
    /// `MEASURE_WIDTH` wide would reach past the space the window's tab bar
    /// leaves them: its right edge, less the items placed at the right, such
    /// as the window buttons.
    fn tab_overflow(&self, ui_items: &[UIItem]) -> f32 {
        let border = self.get_os_border();
        let bar_right = (self.dimensions.pixel_width - border.right.get()) as f32;
        let mut tabs_right = 0f32;
        let mut limit = bar_right;
        for item in ui_items {
            match &item.item_type {
                UIItemType::TabBar(TabBarItem::Tab { .. } | TabBarItem::NewTabButton) => {
                    tabs_right = tabs_right.max((item.x + item.width) as f32)
                }
                UIItemType::TabBar(
                    TabBarItem::RightStatus | TabBarItem::WindowButton(_) | TabBarItem::None,
                ) if (item.width as f32) < MEASURE_WIDTH / 2. => limit -= item.width as f32,
                _ => {}
            }
        }
        tabs_right - limit
    }

    /// Shortens tab titles when the tabs overflow the tab bar, with `slack`
    /// pixels fewer than the tabs appear to have. Every title longer than a
    /// shared width is cut to that width, and shorter titles stay whole,
    /// using the largest width at which the tabs fit. The active tab keeps
    /// its whole title. A cut title keeps at least a few characters, and a
    /// tab with a harness icon then narrows further by sliding its title
    /// under the icon; tabs that still do not fit overflow as before.
    /// Returns the entries with cut titles and how to draw each cut title,
    /// or None when every title fits.
    fn shrink_tab_titles(
        &self,
        items: &[TabEntry],
        ui_items: &[UIItem],
        slack: f32,
    ) -> anyhow::Result<Option<(Vec<TabEntry>, HashMap<usize, TitleCut>)>> {
        let font = self.fonts.title_font()?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let cell_width = metrics.cell_size.width as f32;
        let tab_bar_height = self.tab_bar_pixel_height()?;
        let border = self.get_os_border();
        let bar_width =
            self.dimensions.pixel_width as f32 - (border.left + border.right).get() as f32;

        let mut tabs = vec![];
        let mut other_width = 0.;
        for ui_item in ui_items {
            match &ui_item.item_type {
                UIItemType::TabBar(TabBarItem::Tab { tab_idx, .. }) => {
                    tabs.push((*tab_idx, ui_item.x as f32, ui_item.width as f32))
                }
                UIItemType::TabBar(TabBarItem::LeftStatus) => {}
                // The bar itself spans the whole width.
                UIItemType::TabBar(TabBarItem::None) if ui_item.width as f32 >= bar_width => {}
                UIItemType::TabBar(_) => other_width += ui_item.width as f32,
                _ => {}
            }
        }
        let Some(first_x) = tabs.iter().map(|(_, x, _)| *x).reduce(f32::min) else {
            return Ok(None);
        };
        // A cell of slack absorbs margins that no item reports.
        let available =
            border.left.get() as f32 + bar_width - first_x - other_width - cell_width - slack;
        if tabs.iter().map(|(_, _, width)| width).sum::<f32>() <= available {
            return Ok(None);
        }

        // Each tab's title as cumulative widths at each character boundary,
        // the tab's width beyond its title, and the narrowest the title can
        // become.
        let mut titles = vec![];
        for (tab_idx, _, width) in &tabs {
            let Some(entry) = items.iter().find(
                |entry| matches!(entry.item, TabBarItem::Tab { tab_idx: idx, .. } if idx == *tab_idx),
            ) else {
                return Ok(None);
            };
            let text = entry.title.as_str().into_owned();
            let prefixes = self.title_prefix_widths(&font, &text)?;
            let text_width = prefixes.last().map(|(_, width)| *width).unwrap_or(0.);
            let icon_space = (!entry.icons.is_empty()).then(|| {
                multi_icon_slot_width(tab_bar_height, entry.icons.len())
                    + harness_icon_gap(&metrics)
            });
            let active = matches!(entry.item, TabBarItem::Tab { active: true, .. });
            let min_width = if active {
                text_width
            } else if icon_space.is_some() {
                0.
            } else {
                prefixes
                    .get(MIN_SHRUNK_TITLE_CHARS.min(prefixes.len()).saturating_sub(1))
                    .map(|(_, width)| *width)
                    .unwrap_or(0.)
            };
            titles.push((
                entry,
                prefixes,
                text_width,
                (width - text_width).max(0.),
                min_width,
                icon_space,
            ));
        }

        let widths: Vec<(f32, f32, f32)> = titles
            .iter()
            .map(|(_, _, text_width, overhead, min_width, _)| (*text_width, *overhead, *min_width))
            .collect();
        let cap = shared_title_width(&widths, available);

        let mut shrunk = items.to_vec();
        let mut cuts = HashMap::new();
        for (entry, prefixes, text_width, _, min_width, icon_space) in &titles {
            if *text_width <= cap || *min_width >= *text_width {
                continue;
            }
            let (keep, cut) = cut_title(prefixes, cap, *icon_space);
            let end = keep.checked_sub(1).map(|i| prefixes[i].0).unwrap_or(0);
            let text = entry.title.as_str().into_owned();
            let cells = unicode_column_width(&text[..end], None);
            let TabBarItem::Tab { tab_idx, .. } = entry.item else {
                continue;
            };
            if let Some(target) = shrunk.iter_mut().find(
                |target| matches!(target.item, TabBarItem::Tab { tab_idx: idx, .. } if idx == tab_idx),
            ) {
                target.title.resize(cells, SEQ_ZERO);
                cuts.insert(tab_idx, cut);
            }
        }
        Ok(Some((shrunk, cuts)))
    }

    /// The width of each prefix of a title in the title font, as the byte
    /// offset where the prefix ends and its width in pixels.
    fn title_prefix_widths(
        &self,
        font: &Rc<LoadedFont>,
        text: &str,
    ) -> anyhow::Result<Vec<(usize, f32)>> {
        let window = self.window.as_ref().unwrap().clone();
        let infos = font.shape(
            text,
            move || window.notify(TermWindowNotif::InvalidateShapeCache),
            BlockKey::filter_out_synthetic,
            None,
            wakterm_bidi::Direction::LeftToRight,
            None,
            None,
        )?;
        let mut prefixes: Vec<(usize, f32)> = vec![];
        let mut width = 0.;
        for (i, info) in infos.iter().enumerate() {
            width += info.x_advance.get() as f32;
            let end = infos
                .get(i + 1)
                .map(|next| next.cluster as usize)
                .unwrap_or(text.len());
            match prefixes.last_mut() {
                // Glyphs of one cluster end at the same character boundary.
                Some(last) if last.0 == end => last.1 = width,
                _ => prefixes.push((end, width)),
            }
        }
        Ok(prefixes)
    }

    fn layout_fancy_tab_bar(
        &self,
        palette: &ColorPalette,
        items: &[TabEntry],
        cuts: &HashMap<usize, TitleCut>,
        compact: bool,
        bar_width: f32,
    ) -> anyhow::Result<ComputedElement> {
        let tab_bar_height = self.tab_bar_pixel_height()?;
        let font = self.fonts.title_font()?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let colors = self
            .config
            .colors
            .as_ref()
            .and_then(|c| c.tab_bar.as_ref())
            .cloned()
            .unwrap_or_else(TabBarColors::default);

        let mut left_status = vec![];
        let mut left_eles = vec![];
        let mut right_eles = vec![];
        let bar_colors = ElementColors {
            border: BorderColor::default(),
            bg: if self.focused.is_some() {
                self.config.window_frame.active_titlebar_bg
            } else {
                self.config.window_frame.inactive_titlebar_bg
            }
            .to_linear()
            .into(),
            text: if self.focused.is_some() {
                self.config.window_frame.active_titlebar_fg
            } else {
                self.config.window_frame.inactive_titlebar_fg
            }
            .to_linear()
            .into(),
        };
        let hovered_tab_idx = match self.last_ui_item.as_ref().map(|item| &item.item_type) {
            Some(UIItemType::TabBar(TabBarItem::Tab { tab_idx, .. })) => Some(*tab_idx),
            Some(UIItemType::CloseTab(tab_idx)) => Some(*tab_idx),
            _ => None,
        };

        let item_to_elem = |item: &TabEntry| -> Element {
            let explicit_bg_color = item
                .title_bg
                .or_else(|| first_non_default_background(&item.title))
                .map(|c| palette.resolve_bg(c));
            let explicit_fg_color = item
                .title_fg
                .or_else(|| first_non_default_foreground(&item.title))
                .map(|c| palette.resolve_fg(c));
            let make_title = |forced_text: Option<LinearRgba>| {
                Element::with_line(&font, &item.title, palette).colors(ElementColors {
                    border: BorderColor::default(),
                    bg: explicit_bg_color
                        .map(|c| c.to_linear().into())
                        .unwrap_or(InheritableColor::Inherited),
                    text: forced_text
                        .map(InheritableColor::from)
                        .or_else(|| explicit_fg_color.map(|c| c.to_linear().into()))
                        .unwrap_or(InheritableColor::Inherited),
                })
            };
            // A cut title fades into the tab's background over its last
            // characters, and under its icon, each drawn in a color between
            // the two.
            let fade_title = |title: Element, tab_idx: usize, text: LinearRgba, bg: LinearRgba| {
                let Some(cut) = cuts.get(&tab_idx) else {
                    return title;
                };
                if cut.fades.iter().all(|(_, fade)| *fade <= 0.) {
                    return title;
                }
                let content = item.title.as_str().into_owned();
                let mut parts: Vec<Element> = vec![];
                let mut start = 0;
                let mut run_start = 0;
                let mut run_fade = None;
                let flush = |parts: &mut Vec<Element>, from: usize, to: usize, fade: f32| {
                    if from < to {
                        parts.push(
                            Element::new(
                                &font,
                                ElementContent::Text(content[from..to].to_string()),
                            )
                            .colors(ElementColors {
                                border: BorderColor::default(),
                                bg: InheritableColor::Inherited,
                                text: mix(text, bg, fade).into(),
                            }),
                        );
                    }
                };
                for (end, fade) in &cut.fades {
                    let end = (*end).min(content.len());
                    if run_fade != Some(*fade) {
                        if let Some(previous) = run_fade {
                            flush(&mut parts, run_start, start, previous);
                        }
                        run_start = start;
                        run_fade = Some(*fade);
                    }
                    start = end;
                }
                if let Some(previous) = run_fade {
                    flush(&mut parts, run_start, start, previous);
                }
                Element::new(&font, ElementContent::Children(parts))
            };
            let wrap_icon_title = |title: Element, icon_count: usize, tab_idx: usize| {
                let spacer = match cuts.get(&tab_idx).and_then(|cut| cut.icon_space) {
                    Some(space) => make_harness_icon_spacer(&font, space, 0.),
                    None => make_harness_icon_spacer(
                        &font,
                        multi_icon_slot_width(tab_bar_height, icon_count),
                        harness_icon_gap(&metrics),
                    ),
                };
                Element::new(&font, ElementContent::Children(vec![spacer, title]))
            };

            let new_tab = colors.new_tab();
            let new_tab_hover = colors.new_tab_hover();
            let active_tab = colors.active_tab();

            match item.item {
                TabBarItem::RightStatus | TabBarItem::LeftStatus | TabBarItem::None => {
                    make_title(None)
                        .item_type(UIItemType::TabBar(TabBarItem::None))
                        .line_height(Some(1.2))
                        .margin(BoxDimension {
                            left: Dimension::Cells(0.),
                            right: Dimension::Cells(0.),
                            top: Dimension::Cells(0.0),
                            bottom: Dimension::Cells(0.),
                        })
                        .padding(BoxDimension {
                            left: Dimension::Cells(0.5),
                            right: Dimension::Cells(0.),
                            top: Dimension::Cells(0.),
                            bottom: Dimension::Cells(0.),
                        })
                        .border(BoxDimension::new(Dimension::Pixels(0.)))
                        .colors(bar_colors.clone())
                }
                TabBarItem::NewTabButton => Element::new(
                    &font,
                    ElementContent::Poly {
                        line_width: metrics.underline_height.max(2),
                        poly: SizedPoly {
                            poly: PLUS_BUTTON,
                            width: Dimension::Pixels(metrics.cell_size.height as f32 / 2.),
                            height: Dimension::Pixels(metrics.cell_size.height as f32 / 2.),
                        },
                    },
                )
                .vertical_align(VerticalAlign::Middle)
                .item_type(UIItemType::TabBar(item.item.clone()))
                .margin(BoxDimension {
                    left: Dimension::Cells(0.25),
                    right: Dimension::Cells(0.),
                    top: Dimension::Cells(0.),
                    bottom: Dimension::Cells(0.),
                })
                .padding(BoxDimension {
                    left: Dimension::Cells(0.15),
                    right: Dimension::Cells(0.15),
                    top: Dimension::Cells(0.),
                    bottom: Dimension::Cells(0.05),
                })
                .border(BoxDimension::new(Dimension::Pixels(1.)))
                .colors(ElementColors {
                    border: BorderColor::default(),
                    bg: new_tab.bg_color.to_linear().into(),
                    text: new_tab.fg_color.to_linear().into(),
                })
                .hover_colors(Some(ElementColors {
                    border: BorderColor::default(),
                    bg: new_tab_hover.bg_color.to_linear().into(),
                    text: new_tab_hover.fg_color.to_linear().into(),
                })),
                TabBarItem::Tab { tab_idx, active } if active => {
                    let resolved_bg = explicit_bg_color
                        .or_else(|| {
                            item.assigned_color.map(|color| {
                                tab_render_colors(
                                    color,
                                    colors.background(),
                                    TabColorVisualState::Active,
                                    &self.config.tab_bar_color_intensity,
                                )
                                .bg
                                .into()
                            })
                        })
                        .unwrap_or_else(|| active_tab.bg_color.into())
                        .to_linear();
                    let resolved_text = explicit_fg_color
                        .unwrap_or_else(|| active_tab.fg_color.into())
                        .to_linear();
                    let title = make_title(if item.icons.is_empty() {
                        None
                    } else {
                        Some(resolved_text)
                    });
                    let title = fade_title(title, tab_idx, resolved_text, resolved_bg);
                    let element = if !item.icons.is_empty() {
                        wrap_icon_title(title, item.icons.len(), tab_idx)
                    } else {
                        title
                    };
                    if !item.icons.is_empty()
                        && std::env::var_os("WAKTERM_TRACE_TAB_COLORS").is_some()
                    {
                        log::error!(
                            "fancy_tab_color_trace state=active explicit_fg={:?} wrapper_text={} wrapper_bg={}",
                            explicit_fg_color.map(|c| c.to_string()),
                            linear_to_rgb_hex(resolved_text),
                            linear_to_rgb_hex(resolved_bg),
                        );
                    }
                    element
                        .vertical_align(VerticalAlign::Bottom)
                        .item_type(UIItemType::TabBar(item.item.clone()))
                        .margin(BoxDimension {
                            left: Dimension::Cells(0.),
                            right: Dimension::Cells(0.),
                            top: Dimension::Cells(0.),
                            bottom: Dimension::Cells(0.),
                        })
                        .padding(BoxDimension {
                            left: Dimension::Cells(0.15),
                            right: Dimension::Cells(0.15),
                            top: Dimension::Cells(0.),
                            bottom: Dimension::Cells(0.03),
                        })
                        .border(BoxDimension::new(Dimension::Pixels(1.)))
                        .colors(ElementColors {
                            border: BorderColor::new(resolved_bg),
                            bg: resolved_bg.into(),
                            text: resolved_text.into(),
                        })
                }
                TabBarItem::Tab { tab_idx, .. } => {
                    let hovered = hovered_tab_idx == Some(tab_idx);
                    let visual_state = if hovered {
                        TabColorVisualState::Hover
                    } else {
                        TabColorVisualState::Inactive
                    };
                    let inactive_tab = if hovered {
                        colors.inactive_tab_hover()
                    } else {
                        colors.inactive_tab()
                    };
                    let edge = if hovered {
                        colors.inactive_tab_hover().bg_color.to_linear()
                    } else {
                        colors.inactive_tab_edge().to_linear()
                    };
                    let bg = explicit_bg_color
                        .or_else(|| {
                            item.assigned_color.map(|color| {
                                tab_render_colors(
                                    color,
                                    colors.background(),
                                    visual_state,
                                    &self.config.tab_bar_color_intensity,
                                )
                                .bg
                                .into()
                            })
                        })
                        .unwrap_or_else(|| inactive_tab.bg_color.into())
                        .to_linear();
                    let text = explicit_fg_color
                        .unwrap_or_else(|| inactive_tab.fg_color.into())
                        .to_linear();
                    // A tab whose agents are all working, or optionally whose
                    // reply was reviewed, is darkened toward the tab bar, so
                    // that tabs waiting for the user stand out.
                    let (bg, text) = if self.tab_dimmed(item) {
                        let bg = mix(bg, colors.background().to_linear(), WORKING_TAB_DIM);
                        (bg, mix(text, bg, WORKING_TAB_DIM))
                    } else {
                        (bg, text)
                    };
                    let title = make_title(if item.icons.is_empty() {
                        None
                    } else {
                        Some(text)
                    });
                    let title = fade_title(title, tab_idx, text, bg);
                    let element = if !item.icons.is_empty() {
                        wrap_icon_title(title, item.icons.len(), tab_idx)
                    } else {
                        title
                    };
                    if !item.icons.is_empty()
                        && std::env::var_os("WAKTERM_TRACE_TAB_COLORS").is_some()
                    {
                        log::error!(
                            "fancy_tab_color_trace state={} explicit_fg={:?} wrapper_text={} wrapper_bg={}",
                            if hovered { "hover" } else { "inactive" },
                            explicit_fg_color.map(|c| c.to_string()),
                            linear_to_rgb_hex(text),
                            linear_to_rgb_hex(bg),
                        );
                    }
                    element
                        .vertical_align(VerticalAlign::Bottom)
                        .item_type(UIItemType::TabBar(item.item.clone()))
                        .margin(BoxDimension {
                            left: Dimension::Cells(0.),
                            right: Dimension::Cells(0.),
                            top: Dimension::Cells(0.),
                            bottom: Dimension::Cells(0.),
                        })
                        .padding(BoxDimension {
                            left: Dimension::Cells(0.15),
                            right: Dimension::Cells(0.15),
                            top: Dimension::Cells(0.),
                            bottom: Dimension::Cells(0.03),
                        })
                        .border(BoxDimension::new(Dimension::Pixels(1.)))
                        .colors(ElementColors {
                            border: BorderColor {
                                left: bg,
                                right: edge,
                                top: bg,
                                bottom: bg,
                            },
                            bg: bg.into(),
                            text: text.into(),
                        })
                }
                TabBarItem::WindowButton(button) => window_button_element(
                    button,
                    self.window_state.contains(window::WindowState::MAXIMIZED),
                    &font,
                    &metrics,
                    &self.config,
                ),
            }
        };

        // Reserve space for the native titlebar buttons
        if self
            .config
            .window_decorations
            .contains(::window::WindowDecorations::INTEGRATED_BUTTONS)
            && self.config.integrated_title_button_style == IntegratedTitleButtonStyle::MacOsNative
            && !self.window_state.contains(window::WindowState::FULL_SCREEN)
        {
            left_status.push(
                Element::new(&font, ElementContent::Text("".to_string())).margin(BoxDimension {
                    left: Dimension::Cells(4.0), // FIXME: determine exact width of macos ... buttons
                    right: Dimension::Cells(0.),
                    top: Dimension::Cells(0.),
                    bottom: Dimension::Cells(0.),
                }),
            );
        }

        for item in items {
            match item.item {
                TabBarItem::LeftStatus => left_status.push(item_to_elem(item)),
                TabBarItem::None | TabBarItem::RightStatus => right_eles.push(item_to_elem(item)),
                TabBarItem::WindowButton(_) => {
                    if self.config.integrated_title_button_alignment
                        == IntegratedTitleButtonAlignment::Left
                    {
                        left_eles.push(item_to_elem(item))
                    } else {
                        right_eles.push(item_to_elem(item))
                    }
                }
                TabBarItem::Tab { tab_idx, active } => {
                    let mut elem = item_to_elem(item);
                    elem.content = match elem.content {
                        ElementContent::Text(_) => unreachable!(),
                        ElementContent::Poly { .. } => unreachable!(),
                        ElementContent::Children(mut kids) => {
                            if self.config.show_close_tab_button_in_tabs && (active || !compact) {
                                kids.push(make_x_button(&font, &metrics, &colors, tab_idx, active));
                            }
                            ElementContent::Children(kids)
                        }
                    };
                    left_eles.push(elem);
                }
                _ => left_eles.push(item_to_elem(item)),
            }
        }

        let mut children = vec![];

        if !left_status.is_empty() {
            children.push(
                Element::new(&font, ElementContent::Children(left_status))
                    .colors(bar_colors.clone()),
            );
        }

        let window_buttons_at_left = self
            .config
            .window_decorations
            .contains(window::WindowDecorations::INTEGRATED_BUTTONS)
            && (self.config.integrated_title_button_alignment
                == IntegratedTitleButtonAlignment::Left
                || self.config.integrated_title_button_style
                    == IntegratedTitleButtonStyle::MacOsNative);

        let left_padding = if window_buttons_at_left {
            if self.config.integrated_title_button_style == IntegratedTitleButtonStyle::MacOsNative
            {
                if !self.window_state.contains(window::WindowState::FULL_SCREEN) {
                    Dimension::Pixels(70.0)
                } else {
                    Dimension::Cells(0.5)
                }
            } else {
                Dimension::Pixels(0.0)
            }
        } else {
            Dimension::Cells(0.5)
        };

        children.push(
            Element::new(&font, ElementContent::Children(left_eles))
                .vertical_align(VerticalAlign::Bottom)
                .colors(bar_colors.clone())
                .padding(BoxDimension {
                    left: left_padding,
                    right: Dimension::Cells(0.),
                    top: Dimension::Cells(0.),
                    bottom: Dimension::Cells(0.),
                })
                .zindex(1),
        );
        children.push(
            Element::new(&font, ElementContent::Children(right_eles))
                .colors(bar_colors.clone())
                .float(Float::Right),
        );

        let content = ElementContent::Children(children);

        let tabs = Element::new(&font, content)
            .display(DisplayType::Block)
            .item_type(UIItemType::TabBar(TabBarItem::None))
            .min_width(Some(Dimension::Pixels(bar_width)))
            .min_height(Some(Dimension::Pixels(tab_bar_height)))
            .vertical_align(VerticalAlign::Bottom)
            .colors(bar_colors);

        let border = self.get_os_border();

        let mut computed = self.compute_element(
            &LayoutContext {
                height: DimensionContext {
                    dpi: self.dimensions.dpi as f32,
                    pixel_max: self.dimensions.pixel_height as f32,
                    pixel_cell: metrics.cell_size.height as f32,
                },
                width: DimensionContext {
                    dpi: self.dimensions.dpi as f32,
                    pixel_max: bar_width,
                    pixel_cell: metrics.cell_size.width as f32,
                },
                bounds: euclid::rect(
                    border.left.get() as f32,
                    0.,
                    bar_width - (border.left + border.right).get() as f32,
                    tab_bar_height,
                ),
                metrics: &metrics,
                gl_state: self.render_state.as_ref().unwrap(),
                zindex: 10,
            },
            &tabs,
        )?;

        computed.translate(euclid::vec2(
            0.,
            if self.config.tab_bar_at_bottom {
                self.dimensions.pixel_height as f32
                    - (computed.bounds.height() + border.bottom.get() as f32)
            } else {
                border.top.get() as f32
            },
        ));

        Ok(computed)
    }

    pub fn paint_fancy_tab_bar(&self) -> anyhow::Result<Vec<UIItem>> {
        let computed = self.fancy_tab_bar.as_ref().ok_or_else(|| {
            anyhow::anyhow!("paint_fancy_tab_bar called but fancy_tab_bar is None")
        })?;
        let ui_items = computed.ui_items();

        let gl_state = self.render_state.as_ref().unwrap();
        self.render_element(&computed, gl_state, None)?;
        self.paint_fancy_tab_bar_harness_icons(&ui_items)?;

        Ok(ui_items)
    }

    fn paint_fancy_tab_bar_harness_icons(&self, ui_items: &[UIItem]) -> anyhow::Result<()> {
        let gl_state = self.render_state.as_ref().unwrap();
        let font = self.fonts.title_font()?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let items = self.tab_bar.items();
        let colors = self
            .config
            .colors
            .as_ref()
            .and_then(|c| c.tab_bar.as_ref())
            .cloned()
            .unwrap_or_else(TabBarColors::default);
        let fallback_palette = config::TermConfig::new().color_palette();
        let palette = self.palette.as_ref().unwrap_or(&fallback_palette);
        let layer = gl_state.layer_for_zindex(11)?;
        let mut layers = layer.quad_allocator();
        let width_context = DimensionContext {
            dpi: self.dimensions.dpi as f32,
            pixel_max: self.dimensions.pixel_width as f32,
            pixel_cell: metrics.cell_size.width as f32,
        };
        let tab_left_padding = Dimension::Cells(0.15).evaluate_as_pixels(width_context);
        let hovered_tab_idx = match self.last_ui_item.as_ref().map(|item| &item.item_type) {
            Some(UIItemType::TabBar(TabBarItem::Tab { tab_idx, .. })) => Some(*tab_idx),
            Some(UIItemType::CloseTab(tab_idx)) => Some(*tab_idx),
            _ => None,
        };

        for item in ui_items {
            let tab_idx = match item.item_type {
                UIItemType::TabBar(TabBarItem::Tab { tab_idx, .. }) => tab_idx,
                _ => continue,
            };
            let Some(entry) = items
                .iter()
                .find(|entry| matches!(entry.item, TabBarItem::Tab { tab_idx: idx, .. } if idx == tab_idx))
            else {
                continue;
            };
            if entry.icons.is_empty() {
                continue;
            }

            let hovered = hovered_tab_idx == Some(tab_idx);
            let mut color = harness_icon_color(entry, &colors, palette, hovered);
            if entry.needs_attention && self.config.agent_tab_attention_pulse {
                let cycle = Duration::from_secs(2).as_secs_f32();
                let phase = ATTENTION_PULSE_START.elapsed().as_secs_f32() % cycle / cycle;
                let opacity = 0.5 + 0.5 * (phase * std::f32::consts::TAU).cos();
                color.3 *= opacity;
                self.update_next_frame_time(Some(Instant::now() + Duration::from_millis(33)));
            }
            let working = self.config.agent_tab_dim_working && entry.agents_working;
            let active = matches!(entry.item, TabBarItem::Tab { active: true, .. });
            let marker_color = color;
            if self.tab_dimmed(entry) && !active {
                color.3 *= 1. - WORKING_TAB_DIM;
            }
            let item_height = item.height as f32;
            let single_icon_size = (item_height - 2.0).max(0.0);
            let overlap_stride = single_icon_size * 0.65;

            for (i, icon) in entry.icons.iter().enumerate() {
                let icon_x = item.x as f32
                    + tab_left_padding
                    + (i as f32) * overlap_stride
                    + (harness_icon_slot_width(item_height) - single_icon_size).max(0.0) / 2.0;
                let icon_y = item.y as f32 + (item_height - single_icon_size) / 2.0;
                self.poly_quad(
                    &mut layers,
                    1,
                    euclid::point2(icon_x, icon_y),
                    harness_icon_poly(*icon),
                    metrics.underline_height.max(2),
                    euclid::size2(single_icon_size, single_icon_size),
                    color,
                )?
                .set_grayscale();
            }
            // A working agent's marker blinks at the icon's lower right.
            if working {
                let cycle = WORKING_MARKER_PERIOD.as_secs_f32();
                let phase = ATTENTION_PULSE_START.elapsed().as_secs_f32() % cycle / cycle;
                let mut marker_color = marker_color;
                marker_color.3 *= 0.5 + 0.5 * (phase * std::f32::consts::TAU).cos();
                self.update_next_frame_time(Some(Instant::now() + Duration::from_millis(50)));
                let dot_size = (single_icon_size * 0.24).max(3.0);
                self.poly_quad(
                    &mut layers,
                    1,
                    euclid::point2(
                        item.x as f32 + tab_left_padding + single_icon_size - dot_size * 0.5,
                        item.y as f32 + item_height - dot_size - 1.0,
                    ),
                    ATTENTION_DOT,
                    1,
                    euclid::size2(dot_size, dot_size),
                    marker_color,
                )?;
            }
            if entry.needs_attention && !self.config.agent_tab_attention_pulse {
                let dot_size = (single_icon_size * 0.22).max(3.0);
                self.poly_quad(
                    &mut layers,
                    1,
                    euclid::point2(
                        item.x as f32 + tab_left_padding + single_icon_size - dot_size * 0.5,
                        item.y as f32 + 1.0,
                    ),
                    ATTENTION_DOT,
                    1,
                    euclid::size2(dot_size, dot_size),
                    color,
                )?;
            }
        }

        Ok(())
    }
}

/// The largest width that every title longer than it can be cut to so that
/// the tabs fit in `available` pixels, given each tab's title width, its
/// width beyond the title, and the narrowest its title can become.
fn shared_title_width(widths: &[(f32, f32, f32)], available: f32) -> f32 {
    let fits = |cap: f32| {
        widths
            .iter()
            .map(|(title, overhead, min)| overhead + title.min(cap.max(*min)))
            .sum::<f32>()
            <= available
    };
    let longest = widths.iter().map(|(title, _, _)| *title).fold(0., f32::max);
    let (mut low, mut high) = (0., longest);
    if fits(high) {
        return high;
    }
    while high - low > 0.5 {
        let mid = (low + high) / 2.;
        if fits(mid) {
            low = mid;
        } else {
            high = mid;
        }
    }
    low
}

fn make_x_button(
    font: &Rc<LoadedFont>,
    metrics: &RenderMetrics,
    colors: &TabBarColors,
    tab_idx: usize,
    active: bool,
) -> Element {
    Element::new(
        &font,
        ElementContent::Poly {
            line_width: metrics.underline_height.max(2),
            poly: SizedPoly {
                poly: X_BUTTON,
                width: Dimension::Pixels(metrics.cell_size.height as f32 / 2.),
                height: Dimension::Pixels(metrics.cell_size.height as f32 / 2.),
            },
        },
    )
    // Ensure that we draw our background over the
    // top of the rest of the tab contents
    .zindex(1)
    .vertical_align(VerticalAlign::Middle)
    .float(Float::Right)
    .item_type(UIItemType::CloseTab(tab_idx))
    .hover_colors({
        let inactive_tab_hover = colors.inactive_tab_hover();
        let active_tab = colors.active_tab();

        Some(ElementColors {
            border: BorderColor::default(),
            bg: (if active {
                inactive_tab_hover.bg_color
            } else {
                active_tab.bg_color
            })
            .to_linear()
            .into(),
            text: (if active {
                inactive_tab_hover.fg_color
            } else {
                active_tab.fg_color
            })
            .to_linear()
            .into(),
        })
    })
    .padding(BoxDimension {
        left: Dimension::Cells(0.25),
        right: Dimension::Cells(0.25),
        top: Dimension::Cells(0.25),
        bottom: Dimension::Cells(0.25),
    })
    .margin(BoxDimension {
        left: Dimension::Cells(0.5),
        right: Dimension::Cells(0.),
        top: Dimension::Cells(0.),
        bottom: Dimension::Cells(0.),
    })
}

fn make_harness_icon_spacer(font: &Rc<LoadedFont>, slot_width: f32, gap: f32) -> Element {
    Element::new(font, ElementContent::Children(vec![]))
        .min_width(Some(Dimension::Pixels(slot_width)))
        .margin(BoxDimension {
            left: Dimension::Cells(0.),
            right: Dimension::Pixels(gap),
            top: Dimension::Pixels(0.),
            bottom: Dimension::Cells(0.),
        })
}

fn harness_icon_slot_width(tab_bar_height: f32) -> f32 {
    tab_bar_height * 0.88
}

fn multi_icon_slot_width(tab_bar_height: f32, icon_count: usize) -> f32 {
    let single = harness_icon_slot_width(tab_bar_height);
    if icon_count <= 1 {
        single
    } else {
        let icon_size = (tab_bar_height - 2.0).max(0.0);
        let overlap_stride = icon_size * 0.65;
        single + (icon_count as f32 - 1.0) * overlap_stride
    }
}

fn harness_icon_gap(metrics: &RenderMetrics) -> f32 {
    metrics.cell_size.width as f32 * 0.08
}

fn harness_icon_poly(icon: TabHarnessIcon) -> &'static [Poly] {
    match icon {
        TabHarnessIcon::Agy => HARNESS_ICON_AGY_POLY,
        TabHarnessIcon::Claude => HARNESS_ICON_CLAUDE_POLY,
        TabHarnessIcon::Codex => HARNESS_ICON_CODEX_POLY,
        TabHarnessIcon::Gemini => HARNESS_ICON_GEMINI_POLY,
        TabHarnessIcon::OpenCode => HARNESS_ICON_OPENCODE_POLY,
        TabHarnessIcon::ZCode => HARNESS_ICON_ZCODE_POLY,
    }
}

fn harness_icon_color(
    item: &TabEntry,
    colors: &TabBarColors,
    palette: &ColorPalette,
    hovered: bool,
) -> LinearRgba {
    let fg = item
        .title_fg
        .or_else(|| first_non_default_foreground(&item.title))
        .map(|c| palette.resolve_fg(c).to_linear());

    match item.item {
        TabBarItem::Tab { active: true, .. } => {
            fg.unwrap_or_else(|| colors.active_tab().fg_color.to_linear())
        }
        TabBarItem::Tab { active: false, .. } if hovered => {
            fg.unwrap_or_else(|| colors.inactive_tab_hover().fg_color.to_linear())
        }
        TabBarItem::Tab { active: false, .. } => {
            fg.unwrap_or_else(|| colors.inactive_tab().fg_color.to_linear())
        }
        _ => fg.unwrap_or_default(),
    }
}

fn first_non_default_background(line: &Line) -> Option<ColorAttribute> {
    (0..line.len()).find_map(|idx| {
        line.get_cell(idx)
            .and_then(|cell| match cell.attrs().background() {
                ColorAttribute::Default => None,
                color => Some(color),
            })
    })
}

fn first_non_default_foreground(line: &Line) -> Option<ColorAttribute> {
    (0..line.len()).find_map(|idx| {
        line.get_cell(idx)
            .and_then(|cell| match cell.attrs().foreground() {
                ColorAttribute::Default => None,
                color => Some(color),
            })
    })
}

fn linear_to_rgb_hex(color: LinearRgba) -> String {
    let srgb = color.to_srgb();
    format!(
        "#{:02x}{:02x}{:02x}",
        (srgb.0.clamp(0.0, 1.0) * 255.0).round() as u8,
        (srgb.1.clamp(0.0, 1.0) * 255.0).round() as u8,
        (srgb.2.clamp(0.0, 1.0) * 255.0).round() as u8
    )
}

#[cfg(test)]
mod tests {
    use super::{first_non_default_background, first_non_default_foreground};
    use crate::tabbar::parse_status_text;
    use termwiz::cell::CellAttributes;
    use termwiz::color::AnsiColor;
    use wakterm_term::color::ColorAttribute;

    #[test]
    fn finds_background_after_default_leading_cell() {
        let line = parse_status_text("A\x1b[41mB", CellAttributes::blank());

        assert_eq!(
            line.get_cell(0).unwrap().attrs().background(),
            ColorAttribute::Default
        );
        assert_eq!(
            first_non_default_background(&line),
            Some(AnsiColor::Maroon.into())
        );
    }

    #[test]
    fn finds_foreground_after_default_leading_cell() {
        let line = parse_status_text("A\x1b[32mB", CellAttributes::blank());

        assert_eq!(
            line.get_cell(0).unwrap().attrs().foreground(),
            ColorAttribute::Default
        );
        assert_eq!(
            first_non_default_foreground(&line),
            Some(AnsiColor::Green.into())
        );
    }
}

#[cfg(test)]
mod test {
    use super::{cut_title, shared_title_width, TITLE_FADE_STEP, TITLE_FADE_STEPS};

    #[test]
    fn shared_title_width_cuts_only_the_longest_titles() {
        // Titles of 10, 40 and 100 pixels, each tab 6 pixels wider, none
        // shorter than 5 pixels.
        let widths = [(10., 6., 5.), (40., 6., 5.), (100., 6., 5.)];
        // Everything fits: no title is cut.
        assert_eq!(shared_title_width(&widths, 200.), 100.);
        // 6 * 3 + 10 + 40 + cap = 120 at a cap of 52: only the longest is cut.
        let cap = shared_title_width(&widths, 120.);
        assert!((cap - 52.).abs() <= 0.5, "{}", cap);
        // 6 * 3 + 10 + 2 * cap = 60 at a cap of 16: the short title stays whole.
        let cap = shared_title_width(&widths, 60.);
        assert!((cap - 16.).abs() <= 0.5, "{}", cap);
        // A title that may not shrink, such as the active tab's, keeps its
        // width, and the others make room.
        let widths = [(100., 6., 100.), (100., 6., 0.)];
        let cap = shared_title_width(&widths, 160.);
        assert!((cap - 48.).abs() <= 0.5, "{}", cap);
    }

    /// A title of ten characters, each 10 pixels wide.
    fn prefixes() -> Vec<(usize, f32)> {
        (1..=10).map(|i| (i, i as f32 * 10.)).collect()
    }

    fn fades(cut: &super::TitleCut) -> Vec<f32> {
        cut.fades.iter().map(|(_, fade)| *fade).collect()
    }

    #[test]
    fn a_cut_title_fades_its_last_characters_then_fewer_near_its_shortest() {
        let prefixes = prefixes();
        // Room for 6 characters: 4 full, then the two fade steps.
        let (keep, cut) = cut_title(&prefixes, 65., Some(20.));
        assert_eq!(keep, 6);
        assert_eq!(
            fades(&cut),
            [0., 0., 0., 0., TITLE_FADE_STEPS[0], TITLE_FADE_STEPS[1]]
        );
        assert_eq!(cut.icon_space, None);
        // 4 characters: 3 full and one fading; 3: no fade at all.
        assert_eq!(
            fades(&cut_title(&prefixes, 40., Some(20.)).1),
            [0., 0., 0., TITLE_FADE_STEP]
        );
        assert_eq!(fades(&cut_title(&prefixes, 30., Some(20.)).1), [0., 0., 0.]);
    }

    #[test]
    fn below_its_shortest_a_title_slides_under_its_icon_and_fades() {
        let prefixes = prefixes();
        // 20 pixels of room for the title: the 30-pixel title moves 10 pixels
        // left, so the icon's 20-pixel space narrows to 10 and half covers the
        // first character.
        let (keep, cut) = cut_title(&prefixes, 20., Some(20.));
        assert_eq!(keep, 3);
        assert_eq!(cut.icon_space, Some(10.));
        assert_eq!(fades(&cut), [1., 0., 0.]);
        let (_, cut) = cut_title(&prefixes, 25., Some(20.));
        assert_eq!(cut.icon_space, Some(15.));
        assert_eq!(fades(&cut), [0.5, 0., 0.]);
        // No room for the title: only the icon's space is left, and whatever
        // text remains under it is fully faded.
        let (keep, cut) = cut_title(&prefixes, 0., Some(20.));
        assert_eq!(keep, 2);
        assert_eq!(cut.icon_space, Some(0.));
        assert_eq!(fades(&cut), [1., 1.]);
        // Without an icon the title stops at its shortest.
        let (keep, cut) = cut_title(&prefixes, 0., None);
        assert_eq!((keep, fades(&cut)), (3, vec![0., 0., 0.]));
    }
}
