use crate::color::LinearRgba;
use crate::customglyph::*;
use crate::tabbar::{TabBarItem, TabEntry};
use crate::termwindow::box_model::*;
use crate::termwindow::render::window_buttons::window_button_element;
use crate::termwindow::{MouseCapture, TabHarnessIcon, UIItem, UIItemType};
use crate::utilsprites::RenderMetrics;
use config::{Dimension, DimensionContext, TabBarColors};
use std::rc::Rc;
use wakterm_font::LoadedFont;
use wakterm_term::color::{ColorAttribute, ColorPalette};
use wakterm_term::Line;
use wakterm_term::TerminalConfiguration;
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
        let tab_bar_height = self.tab_bar_pixel_height()?;
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

        let item_to_elem = |item: &TabEntry| -> Element {
            let title = Element::with_line(&font, &item.title, palette);
            let element = match item.item {
                TabBarItem::Tab { .. } => {
                    if item.icon.is_some() {
                        Element::new(
                            &font,
                            ElementContent::Children(vec![
                                make_harness_icon_spacer(
                                    &font,
                                    harness_icon_slot_width(tab_bar_height),
                                    harness_icon_gap(&metrics),
                                ),
                                title,
                            ]),
                        )
                    } else {
                        title
                    }
                }
                _ => title,
            };

            let bg_color = item
                .title_bg
                .or_else(|| first_non_default_background(&item.title))
                .map(|c| palette.resolve_bg(c));

            let new_tab = colors.new_tab();
            let new_tab_hover = colors.new_tab_hover();
            let active_tab = colors.active_tab();

            match item.item {
                TabBarItem::RightStatus | TabBarItem::LeftStatus | TabBarItem::None => element
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
                    .colors(bar_colors.clone()),
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
                TabBarItem::Tab { active, .. } if active => element
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
                        border: BorderColor::new(
                            bg_color
                                .unwrap_or_else(|| active_tab.bg_color.into())
                                .to_linear(),
                        ),
                        bg: bg_color
                            .unwrap_or_else(|| active_tab.bg_color.into())
                            .to_linear()
                            .into(),
                        text: active_tab.fg_color.to_linear().into(),
                    }),
                TabBarItem::Tab { .. } => element
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
                    .colors({
                        let inactive_tab = colors.inactive_tab();
                        let bg = bg_color
                            .unwrap_or_else(|| inactive_tab.bg_color.into())
                            .to_linear();
                        let edge = colors.inactive_tab_edge().to_linear();
                        ElementColors {
                            border: BorderColor {
                                left: bg,
                                right: edge,
                                top: bg,
                                bottom: bg,
                            },
                            bg: bg.into(),
                            text: inactive_tab.fg_color.to_linear().into(),
                        }
                    })
                    .hover_colors({
                        let inactive_tab_hover = colors.inactive_tab_hover();
                        Some(ElementColors {
                            border: BorderColor::new(
                                bg_color
                                    .unwrap_or_else(|| inactive_tab_hover.bg_color.into())
                                    .to_linear(),
                            ),
                            bg: bg_color
                                .unwrap_or_else(|| inactive_tab_hover.bg_color.into())
                                .to_linear()
                                .into(),
                            text: inactive_tab_hover.fg_color.to_linear().into(),
                        })
                    }),
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
        let tab_bar_height = self.tab_bar_pixel_height()?;
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
        let icon_slot_width = harness_icon_slot_width(tab_bar_height);

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
            let Some(icon) = entry.icon else {
                continue;
            };

            let hovered = self.current_mouse_event.as_ref().is_some_and(|event| {
                item.hit_test(event.coords.x, event.coords.y)
                    && matches!(self.current_mouse_capture, None | Some(MouseCapture::UI))
            });
            let color = harness_icon_color(entry, &colors, palette, hovered);
            let item_height = item.height as f32;
            let icon_size = (item_height - 2.0).max(0.0);
            let icon_x =
                item.x as f32 + tab_left_padding + (icon_slot_width - icon_size).max(0.0) / 2.0;
            let icon_y = item.y as f32 + (item_height - icon_size) / 2.0;
            self.poly_quad(
                &mut layers,
                1,
                euclid::point2(icon_x, icon_y),
                harness_icon_poly(icon),
                metrics.underline_height.max(2),
                euclid::size2(icon_size, icon_size),
                color,
            )?;
        }

        Ok(())
    }
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

fn make_harness_icon_spacer(
    font: &Rc<LoadedFont>,
    slot_width: f32,
    gap: f32,
) -> Element {
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

fn harness_icon_gap(metrics: &RenderMetrics) -> f32 {
    metrics.cell_size.width as f32 * 0.08
}

fn harness_icon_poly(icon: TabHarnessIcon) -> &'static [Poly] {
    match icon {
        TabHarnessIcon::Claude => HARNESS_ICON_CLAUDE_POLY,
        TabHarnessIcon::Codex => HARNESS_ICON_CODEX_POLY,
        TabHarnessIcon::Gemini => HARNESS_ICON_GEMINI_POLY,
        TabHarnessIcon::OpenCode => HARNESS_ICON_OPENCODE_POLY,
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
        TabBarItem::Tab { active: false, .. } if hovered => fg
            .unwrap_or_else(|| colors.inactive_tab_hover().fg_color.to_linear()),
        TabBarItem::Tab { active: false, .. } => {
            fg.unwrap_or_else(|| colors.inactive_tab().fg_color.to_linear())
        }
        _ => fg.unwrap_or_default(),
    }
}

fn first_non_default_background(line: &Line) -> Option<ColorAttribute> {
    (0..line.len()).find_map(|idx| {
        line.get_cell(idx).and_then(|cell| match cell.attrs().background() {
            ColorAttribute::Default => None,
            color => Some(color),
        })
    })
}

fn first_non_default_foreground(line: &Line) -> Option<ColorAttribute> {
    (0..line.len()).find_map(|idx| {
        line.get_cell(idx).and_then(|cell| match cell.attrs().foreground() {
            ColorAttribute::Default => None,
            color => Some(color),
        })
    })
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
