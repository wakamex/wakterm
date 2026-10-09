use crate::color::LinearRgba;
use crate::customglyph::*;
use crate::quad::QuadTrait;
use crate::tab_colors::{tab_render_colors, TabColorVisualState};
use crate::tabbar::{TabBarItem, TabEntry};
use crate::termwindow::box_model::*;
use crate::termwindow::render::window_buttons::window_button_element;
use crate::termwindow::TermWindowNotif;
use crate::termwindow::{TabHarnessIcon, UIItem, UIItemType};
use crate::utilsprites::RenderMetrics;
use config::{Dimension, DimensionContext, TabBarColors};
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use termwiz::cell::unicode_column_width;
use termwiz::surface::SEQ_ZERO;
use wakterm_font::LoadedFont;
use wakterm_term::color::{ColorAttribute, ColorPalette};
use wakterm_term::{Line, TerminalConfiguration};
use window::WindowOps;
use window::{IntegratedTitleButtonAlignment, IntegratedTitleButtonStyle};

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

/// How far toward the tab's background each of a cut title's last
/// characters is drawn, and how many cells of a cut title remain at least.
const TITLE_FADE_STEPS: [f32; 2] = [0.45, 0.75];
const MIN_SHRUNK_TITLE_CELLS: f32 = 3.;

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
    pub fn invalidate_fancy_tab_bar(&mut self) {
        self.fancy_tab_bar.take();
    }

    pub fn build_fancy_tab_bar(&self, palette: &ColorPalette) -> anyhow::Result<ComputedElement> {
        let items = self.tab_bar.items();
        let full = self.layout_fancy_tab_bar(palette, items, &[])?;
        if !self.config.tab_titles_shrink_to_fit {
            return Ok(full);
        }
        let full_ui_items = full.ui_items();
        // The shared width comes from an estimate of the space the tabs
        // take; whatever still overflows after layout is measured and taken
        // off the space before the next pass.
        let mut computed = full;
        let mut slack = 0.;
        for _ in 0..3 {
            let Some((shrunk, faded)) = self.shrink_tab_titles(items, &full_ui_items, slack)?
            else {
                break;
            };
            computed = self.layout_fancy_tab_bar(palette, &shrunk, &faded)?;
            let overflow = self.tab_overflow(&computed.ui_items());
            if overflow <= 0. {
                break;
            }
            slack += overflow;
        }
        Ok(computed)
    }

    /// How far, in pixels, the tabs and the new tab button reach past the
    /// space the tab bar leaves them: its right edge, or the leftmost item
    /// placed at the right, such as the window buttons.
    fn tab_overflow(&self, ui_items: &[UIItem]) -> f32 {
        let border = self.get_os_border();
        let bar_right = (self.dimensions.pixel_width - border.right.get()) as f32;
        let mut tabs_right = 0f32;
        let mut limit = bar_right;
        for item in ui_items {
            let right = (item.x + item.width) as f32;
            match &item.item_type {
                UIItemType::TabBar(TabBarItem::Tab { .. } | TabBarItem::NewTabButton) => {
                    tabs_right = tabs_right.max(right)
                }
                UIItemType::TabBar(TabBarItem::RightStatus | TabBarItem::WindowButton(_)) => {
                    limit = limit.min(item.x as f32)
                }
                _ => {}
            }
        }
        tabs_right - limit
    }

    /// Shortens tab titles when the tabs overflow the tab bar, with `slack`
    /// pixels fewer than the tabs appear to have. Every title
    /// longer than a shared width is cut to that width, and shorter titles
    /// stay whole, using the largest width at which the tabs fit. Each cut
    /// title keeps at least a few characters; tabs that still do not fit
    /// overflow as before. Returns the entries with cut titles and the
    /// indices of the tabs that were cut, or None when every title fits.
    fn shrink_tab_titles(
        &self,
        items: &[TabEntry],
        ui_items: &[UIItem],
        slack: f32,
    ) -> anyhow::Result<Option<(Vec<TabEntry>, Vec<usize>)>> {
        let font = self.fonts.title_font()?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let cell_width = metrics.cell_size.width as f32;
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
        // and the tab's width beyond its title.
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
            titles.push((
                entry,
                text,
                prefixes,
                text_width,
                (width - text_width).max(0.),
            ));
        }

        let widths: Vec<(f32, f32)> = titles
            .iter()
            .map(|(_, _, _, text_width, overhead)| (*text_width, *overhead))
            .collect();
        let cap = shared_title_width(&widths, available, MIN_SHRUNK_TITLE_CELLS * cell_width);

        let mut shrunk = items.to_vec();
        let mut faded = vec![];
        for (entry, text, prefixes, text_width, _) in &titles {
            if *text_width <= cap {
                continue;
            }
            let keep = prefixes
                .iter()
                .take_while(|(_, width)| *width <= cap)
                .last()
                .map(|(end, _)| *end)
                .unwrap_or(0);
            let cells = unicode_column_width(&text[..keep], None);
            let TabBarItem::Tab { tab_idx, .. } = entry.item else {
                continue;
            };
            if let Some(target) = shrunk.iter_mut().find(
                |target| matches!(target.item, TabBarItem::Tab { tab_idx: idx, .. } if idx == tab_idx),
            ) {
                target.title.resize(cells, SEQ_ZERO);
                faded.push(tab_idx);
            }
        }
        Ok(Some((shrunk, faded)))
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
        faded: &[usize],
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
            // characters, which are drawn in colors between the two.
            let fade_title = |title: Element, tab_idx: usize, text: LinearRgba, bg: LinearRgba| {
                if !faded.contains(&tab_idx) {
                    return title;
                }
                let content = item.title.as_str().into_owned();
                let tail_start = content
                    .char_indices()
                    .rev()
                    .nth(TITLE_FADE_STEPS.len() - 1)
                    .map(|(index, _)| index)
                    .unwrap_or(0);
                let mut parts = vec![Element::new(
                    &font,
                    ElementContent::Text(content[..tail_start].to_string()),
                )
                .colors(ElementColors {
                    border: BorderColor::default(),
                    bg: InheritableColor::Inherited,
                    text: text.into(),
                })];
                let tail = content[tail_start..].chars();
                let steps = &TITLE_FADE_STEPS[TITLE_FADE_STEPS.len() - tail.clone().count()..];
                for (c, step) in tail.zip(steps) {
                    parts.push(
                        Element::new(&font, ElementContent::Text(c.to_string())).colors(
                            ElementColors {
                                border: BorderColor::default(),
                                bg: InheritableColor::Inherited,
                                text: mix(text, bg, *step).into(),
                            },
                        ),
                    );
                }
                Element::new(&font, ElementContent::Children(parts))
            };
            let wrap_icon_title = |title: Element, icon_count: usize| {
                Element::new(
                    &font,
                    ElementContent::Children(vec![
                        make_harness_icon_spacer(
                            &font,
                            multi_icon_slot_width(tab_bar_height, icon_count),
                            harness_icon_gap(&metrics),
                        ),
                        title,
                    ]),
                )
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
                        wrap_icon_title(title, item.icons.len())
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
                    let title = make_title(if item.icons.is_empty() {
                        None
                    } else {
                        Some(text)
                    });
                    let title = fade_title(title, tab_idx, text, bg);
                    let element = if !item.icons.is_empty() {
                        wrap_icon_title(title, item.icons.len())
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
                            if self.config.show_close_tab_button_in_tabs {
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
            .min_width(Some(Dimension::Pixels(self.dimensions.pixel_width as f32)))
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
                    pixel_max: self.dimensions.pixel_width as f32,
                    pixel_cell: metrics.cell_size.width as f32,
                },
                bounds: euclid::rect(
                    border.left.get() as f32,
                    0.,
                    self.dimensions.pixel_width as f32 - (border.left + border.right).get() as f32,
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
/// the tabs fit in `available` pixels, given each tab's title width and its
/// width beyond the title. Never less than `min_width`.
fn shared_title_width(widths: &[(f32, f32)], available: f32, min_width: f32) -> f32 {
    let fits = |cap: f32| {
        widths
            .iter()
            .map(|(title, overhead)| overhead + title.min(cap))
            .sum::<f32>()
            <= available
    };
    let longest = widths.iter().map(|(title, _)| *title).fold(0., f32::max);
    let (mut low, mut high) = (min_width, longest.max(min_width));
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
    use super::shared_title_width;

    #[test]
    fn shared_title_width_cuts_only_the_longest_titles() {
        // Titles of 10, 40 and 100 pixels, each tab 6 pixels wider.
        let widths = [(10., 6.), (40., 6.), (100., 6.)];
        // Everything fits: no title is cut.
        assert_eq!(shared_title_width(&widths, 200., 5.), 100.);
        // 6 * 3 + 10 + 40 + cap = 120 at a cap of 52: only the longest is cut.
        let cap = shared_title_width(&widths, 120., 5.);
        assert!((cap - 52.).abs() <= 0.5, "{}", cap);
        // 6 * 3 + 10 + 2 * cap = 60 at a cap of 16: the short title stays whole.
        let cap = shared_title_width(&widths, 60., 5.);
        assert!((cap - 16.).abs() <= 0.5, "{}", cap);
        // Too narrow even at the minimum: the minimum is kept.
        assert_eq!(shared_title_width(&widths, 10., 5.), 5.);
    }
}
