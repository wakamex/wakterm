use crate::color::RgbColor;
use crate::{Sixel, SixelData};

const MAX_PARAMS: usize = 5;
const MAX_SIXEL_INPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_SIXEL_DATA_ITEMS: usize = 4 * 1024 * 1024;
const MAX_SIXEL_EXPANDED_SAMPLES: usize = 16 * 1024 * 1024;
const MAX_SIXEL_PIXELS: usize = 16 * 1024 * 1024;

pub struct SixelBuilder {
    pub sixel: Sixel,
    params: [i64; MAX_PARAMS],
    param_no: usize,
    current_command: u8,
    input_bytes: usize,
    expanded_samples: usize,
    truncated: bool,
}

impl SixelBuilder {
    pub fn new(params: &[i64]) -> Self {
        let pan = match params.get(0).unwrap_or(&0) {
            7 | 8 | 9 => 1,
            0 | 1 | 5 | 6 => 2,
            3 | 4 => 3,
            2 => 5,
            _ => 2,
        };
        let background_is_transparent = match params.get(1).unwrap_or(&0) {
            1 => true,
            _ => false,
        };
        let horizontal_grid_size = params.get(2).map(|&x| x);

        Self {
            sixel: Sixel {
                pan,
                pad: 1,
                pixel_width: None,
                pixel_height: None,
                background_is_transparent,
                horizontal_grid_size,
                data: vec![],
            },
            param_no: 0,
            params: [-1; MAX_PARAMS],
            current_command: 0,
            input_bytes: 0,
            expanded_samples: 0,
            truncated: false,
        }
    }

    pub fn push(&mut self, data: u8) {
        if self.input_bytes >= MAX_SIXEL_INPUT_BYTES {
            self.truncated = true;
            return;
        }
        self.input_bytes += 1;
        if self.truncated {
            return;
        }

        match data {
            b'$' => {
                self.finish_command();
                self.push_item(SixelData::CarriageReturn, 0);
            }
            b'-' => {
                self.finish_command();
                self.push_item(SixelData::NewLine, 0);
            }
            0x3f..=0x7e if self.current_command == b'!' => {
                let repeat_count = match u32::try_from(self.params[0]) {
                    Ok(0) | Err(_) => {
                        self.truncated = true;
                        self.finish_command();
                        return;
                    }
                    Ok(repeat_count) => repeat_count,
                };
                self.push_item(
                    SixelData::Repeat {
                        repeat_count,
                        data: data - 0x3f,
                    },
                    repeat_count as usize,
                );
                self.finish_command();
            }
            0x3f..=0x7e => {
                self.finish_command();
                self.push_item(SixelData::Data(data - 0x3f), 1);
            }
            b'#' | b'!' | b'"' => {
                self.finish_command();
                self.current_command = data;
            }
            b'0'..=b'9' if self.current_command != 0 => {
                let pos = self.param_no;
                if pos >= MAX_PARAMS {
                    return;
                }
                if self.params[pos] == -1 {
                    self.params[pos] = 0;
                }
                self.params[pos] = self.params[pos]
                    .saturating_mul(10)
                    .saturating_add((data - b'0') as i64);
            }
            b';' if self.current_command != 0 => {
                let pos = self.param_no;
                if pos >= MAX_PARAMS {
                    return;
                }
                self.param_no += 1;
            }
            _ => {
                // Invalid; break out of current command
                self.finish_command();
            }
        }
    }

    fn push_item(&mut self, item: SixelData, expanded_samples: usize) {
        let next_work = self.expanded_samples.saturating_add(expanded_samples);
        if self.sixel.data.len() >= MAX_SIXEL_DATA_ITEMS || next_work > MAX_SIXEL_EXPANDED_SAMPLES {
            self.truncated = true;
            return;
        }
        self.expanded_samples = next_work;
        self.sixel.data.push(item);
    }

    fn finish_command(&mut self) {
        if self.truncated {
            self.param_no = 0;
            self.params = [-1; MAX_PARAMS];
            self.current_command = 0;
            return;
        }
        if self.sixel.data.len() >= MAX_SIXEL_DATA_ITEMS {
            self.truncated = true;
            return;
        }
        match self.current_command {
            b'#' if self.param_no >= 4 => {
                // Define a color
                let color_number = self.params[0] as u16;
                let system = self.params[1] as u16;
                let a = self.params[2] as u16;
                let b = self.params[3] as u8;
                let c = self.params[4] as u8;

                if system == 1 {
                    self.sixel.data.push(SixelData::DefineColorMapHSL {
                        color_number,
                        hue_angle: a,
                        lightness: b,
                        saturation: c,
                    });
                } else {
                    let r = a as f32 * 255.0 / 100.;
                    let g = b as f32 * 255.0 / 100.;
                    let b = c as f32 * 255.0 / 100.;
                    let rgb = RgbColor::new_8bpc(r as u8, g as u8, b as u8); // FIXME: from linear
                    self.sixel
                        .data
                        .push(SixelData::DefineColorMapRGB { color_number, rgb });
                }
            }
            b'#' => {
                // Use a color
                let color_number = self.params[0] as u16;

                self.sixel
                    .data
                    .push(SixelData::SelectColorMapEntry(color_number));
            }
            b'"' => {
                // Set raster attributes
                let pan = if self.params[0] == -1 {
                    2
                } else {
                    self.params[0]
                };
                let pad = if self.params[1] == -1 {
                    1
                } else {
                    self.params[1]
                };
                let pixel_width = self.params[2];
                let pixel_height = self.params[3];

                self.sixel.pan = pan;
                self.sixel.pad = pad;

                if self.param_no >= 3 {
                    self.sixel.pixel_width.replace(pixel_width as u32);
                    self.sixel.pixel_height.replace(pixel_height as u32);

                    let (size, overflow) =
                        (pixel_width as usize).overflowing_mul(pixel_height as usize);

                    // Ideally we'd just use `try_reserve` here, but that is
                    // nightly Rust only at the time of writing this comment:
                    // <https://github.com/rust-lang/rust/issues/48043>
                    if size > MAX_SIXEL_PIXELS || overflow {
                        log::error!(
                            "Ignoring sixel data {}x{} because {} pixels \
                             either overflows or exceeds the max allowed {}",
                            pixel_width,
                            pixel_height,
                            size,
                            MAX_SIXEL_PIXELS
                        );
                        self.truncated = true;
                        return;
                    }
                }
            }
            _ => {}
        }
        self.param_no = 0;
        self.params = [-1; MAX_PARAMS];
        self.current_command = 0;
    }

    pub fn finish(&mut self) -> bool {
        self.finish_command();
        if self.truncated {
            self.sixel.data.clear();
            false
        } else {
            true
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::parser::Parser;
    use crate::{Action, Esc, EscCode};
    use alloc::boxed::Box;
    use k9::assert_equal as assert_eq;

    #[test]
    fn sixel() {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(b"\x1bP1;2;3;q@\x1b\\");
        assert_eq!(
            vec![
                Action::Sixel(Box::new(Sixel {
                    pan: 2,
                    pad: 1,
                    pixel_width: None,
                    pixel_height: None,
                    background_is_transparent: false,
                    horizontal_grid_size: Some(3),
                    data: vec![SixelData::Data(1)]
                })),
                Action::Esc(Esc::Code(EscCode::StringTerminator)),
            ],
            actions
        );

        assert_eq!(format!("{}", actions[0]), "\x1bP0;0;3q@");

        // This is the "HI" example from wikipedia
        let mut p = Parser::new();
        let actions = p.parse_as_vec(
            b"\x1bPq\
        #0;2;0;0;0#1;2;100;100;0#2;2;0;100;0\
        #1~~@@vv@@~~@@~~$\
        #2??}}GG}}??}}??-\
        #1!14@\
        \x1b\\",
        );

        assert_eq!(
            format!("{}", actions[0]),
            "\x1bP0;0q\
        #0;2;0;0;0#1;2;100;100;0#2;2;0;100;0\
        #1~~@@vv@@~~@@~~$\
        #2??}}GG}}??}}??-\
        #1!14@"
        );

        use SixelData::*;
        assert_eq!(
            vec![
                Action::Sixel(Box::new(Sixel {
                    pan: 2,
                    pad: 1,
                    pixel_width: None,
                    pixel_height: None,
                    background_is_transparent: false,
                    horizontal_grid_size: None,
                    data: vec![
                        DefineColorMapRGB {
                            color_number: 0,
                            rgb: RgbColor::new_8bpc(0, 0, 0)
                        },
                        DefineColorMapRGB {
                            color_number: 1,
                            rgb: RgbColor::new_8bpc(255, 255, 0)
                        },
                        DefineColorMapRGB {
                            color_number: 2,
                            rgb: RgbColor::new_8bpc(0, 255, 0)
                        },
                        SelectColorMapEntry(1),
                        Data(63),
                        Data(63),
                        Data(1),
                        Data(1),
                        Data(55),
                        Data(55),
                        Data(1),
                        Data(1),
                        Data(63),
                        Data(63),
                        Data(1),
                        Data(1),
                        Data(63),
                        Data(63),
                        CarriageReturn,
                        SelectColorMapEntry(2),
                        Data(0),
                        Data(0),
                        Data(62),
                        Data(62),
                        Data(8),
                        Data(8),
                        Data(62),
                        Data(62),
                        Data(0),
                        Data(0),
                        Data(62),
                        Data(62),
                        Data(0),
                        Data(0),
                        NewLine,
                        SelectColorMapEntry(1),
                        Repeat {
                            repeat_count: 14,
                            data: 1
                        }
                    ]
                })),
                Action::Esc(Esc::Code(EscCode::StringTerminator)),
            ],
            actions
        );
    }
}
