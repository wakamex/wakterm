use anyhow::{bail, Context};
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

const RGB_COLOR_COUNT: u32 = 1 << 24;
const DEFAULT_COUNT: usize = 512;
const DEFAULT_OUTPUT: &str = "wakterm-gui/src/tab_color_palette.rs";
const INACTIVE_INTENSITY: f32 = 0.4;

#[derive(Clone, Copy, Debug)]
struct Lab {
    l: f32,
    a: f32,
    b: f32,
}

impl Lab {
    fn chroma_squared(self) -> f32 {
        self.a * self.a + self.b * self.b
    }

    fn distance_squared(self, other: Self) -> f32 {
        let dl = self.l - other.l;
        let da = self.a - other.a;
        let db = self.b - other.b;
        dl * dl + da * da + db * db
    }
}

#[derive(Clone, Copy)]
struct Scheme {
    name: &'static str,
    constant_name: &'static str,
    min_lightness: f32,
    max_lightness: f32,
    min_chroma: f32,
    max_chroma: f32,
}

impl Scheme {
    fn accepts(self, lab: Lab) -> bool {
        let chroma_squared = lab.chroma_squared();
        (self.min_lightness..=self.max_lightness).contains(&lab.l)
            && chroma_squared >= self.min_chroma * self.min_chroma
            && chroma_squared <= self.max_chroma * self.max_chroma
    }

    fn description(self) -> String {
        format!(
            "Oklab L {:.2}..{:.2}, C {:.2}..{:.2}, inactive intensity {:.1}",
            self.min_lightness,
            self.max_lightness,
            self.min_chroma,
            self.max_chroma,
            INACTIVE_INTENSITY
        )
    }
}

const SCHEMES: &[Scheme] = &[
    Scheme {
        name: "dark",
        constant_name: "DARK",
        min_lightness: 0.60,
        max_lightness: 0.82,
        min_chroma: 0.10,
        max_chroma: 0.19,
    },
    Scheme {
        name: "light",
        constant_name: "LIGHT",
        min_lightness: 0.80,
        max_lightness: 0.90,
        min_chroma: 0.02,
        max_chroma: 0.16,
    },
    Scheme {
        name: "mixed",
        constant_name: "MIXED",
        min_lightness: 0.60,
        max_lightness: 0.90,
        min_chroma: 0.02,
        max_chroma: 0.19,
    },
];

struct LinearLookup {
    base: [f32; 256],
    inactive: [f32; 256],
}

impl LinearLookup {
    fn new() -> Self {
        let mut base = [0.0; 256];
        let mut inactive = [0.0; 256];
        for value in 0..256 {
            let srgb = value as f32 / 255.0;
            base[value] = srgb_channel_to_linear(srgb);
            inactive[value] = srgb_channel_to_linear(srgb * INACTIVE_INTENSITY);
        }
        Self { base, inactive }
    }

    fn base_lab(&self, rgb: u32) -> Lab {
        self.lab_from_lookup(rgb, &self.base)
    }

    fn inactive_lab(&self, rgb: u32) -> Lab {
        self.lab_from_lookup(rgb, &self.inactive)
    }

    fn lab_from_lookup(&self, rgb: u32, lookup: &[f32; 256]) -> Lab {
        let red = lookup[((rgb >> 16) & 0xff) as usize];
        let green = lookup[((rgb >> 8) & 0xff) as usize];
        let blue = lookup[(rgb & 0xff) as usize];
        oklab_from_linear_rgb(red, green, blue)
    }
}

struct Candidates {
    rgb: Vec<u32>,
    l: Vec<f32>,
    a: Vec<f32>,
    b: Vec<f32>,
    min_distance: Vec<f32>,
    selected: Vec<u8>,
}

#[derive(Clone, Copy)]
struct Choice {
    index: usize,
    rgb: u32,
    distance: f32,
}

impl Candidates {
    fn build(scheme: Scheme, lookup: &LinearLookup) -> Self {
        let values: Vec<(u32, Lab)> = (0..RGB_COLOR_COUNT)
            .into_par_iter()
            .filter_map(|rgb| {
                scheme
                    .accepts(lookup.base_lab(rgb))
                    .then(|| (rgb, lookup.inactive_lab(rgb)))
            })
            .collect();

        let mut candidates = Self {
            rgb: Vec::with_capacity(values.len()),
            l: Vec::with_capacity(values.len()),
            a: Vec::with_capacity(values.len()),
            b: Vec::with_capacity(values.len()),
            min_distance: vec![f32::INFINITY; values.len()],
            selected: vec![0; values.len()],
        };
        for (rgb, lab) in values {
            candidates.rgb.push(rgb);
            candidates.l.push(lab.l);
            candidates.a.push(lab.a);
            candidates.b.push(lab.b);
        }
        candidates
    }

    fn lab(&self, index: usize) -> Lab {
        Lab {
            l: self.l[index],
            a: self.a[index],
            b: self.b[index],
        }
    }

    fn select_initial_from_center(&mut self, center: Lab) -> Choice {
        let choice = (0..self.rgb.len())
            .into_par_iter()
            .map(|index| Choice {
                index,
                rgb: self.rgb[index],
                distance: self.lab(index).distance_squared(center),
            })
            .reduce_with(better_choice)
            .expect("scheme has eligible colors");
        self.selected[choice.index] = 1;
        choice
    }

    fn bounding_box_center(&self) -> Lab {
        let (mut min_l, mut max_l) = (f32::INFINITY, f32::NEG_INFINITY);
        let (mut min_a, mut max_a) = (f32::INFINITY, f32::NEG_INFINITY);
        let (mut min_b, mut max_b) = (f32::INFINITY, f32::NEG_INFINITY);
        for index in 0..self.rgb.len() {
            min_l = min_l.min(self.l[index]);
            max_l = max_l.max(self.l[index]);
            min_a = min_a.min(self.a[index]);
            max_a = max_a.max(self.a[index]);
            min_b = min_b.min(self.b[index]);
            max_b = max_b.max(self.b[index]);
        }
        Lab {
            l: (min_l + max_l) / 2.0,
            a: (min_a + max_a) / 2.0,
            b: (min_b + max_b) / 2.0,
        }
    }

    fn farthest_current(&self) -> Choice {
        self.min_distance
            .par_iter()
            .enumerate()
            .filter_map(|(index, distance)| {
                (self.selected[index] == 0).then_some(Choice {
                    index,
                    rgb: self.rgb[index],
                    distance: *distance,
                })
            })
            .reduce_with(better_choice)
            .expect("unselected candidate remains")
    }

    fn update_with_center_and_choose(&mut self, center: Lab) -> Choice {
        let rgb = &self.rgb;
        let l = &self.l;
        let a = &self.a;
        let b = &self.b;
        let selected = &self.selected;
        self.min_distance
            .par_iter_mut()
            .enumerate()
            .filter_map(|(index, min_distance)| {
                if selected[index] != 0 {
                    return None;
                }
                let candidate = Lab {
                    l: l[index],
                    a: a[index],
                    b: b[index],
                };
                *min_distance = min_distance.min(candidate.distance_squared(center));
                Some(Choice {
                    index,
                    rgb: rgb[index],
                    distance: *min_distance,
                })
            })
            .reduce_with(better_choice)
            .expect("unselected candidate remains")
    }

    fn mark_selected(&mut self, choice: Choice) {
        self.selected[choice.index] = 1;
    }
}

fn better_choice(left: Choice, right: Choice) -> Choice {
    match left.distance.total_cmp(&right.distance) {
        std::cmp::Ordering::Greater => left,
        std::cmp::Ordering::Less => right,
        std::cmp::Ordering::Equal if left.rgb <= right.rgb => left,
        std::cmp::Ordering::Equal => right,
    }
}

fn generate_scheme(scheme: Scheme, count: usize, lookup: &LinearLookup) -> Vec<u32> {
    let started = Instant::now();
    let mut candidates = Candidates::build(scheme, lookup);
    eprintln!(
        "{}: {} eligible RGB8 colors in {:.2?}",
        scheme.name,
        candidates.rgb.len(),
        started.elapsed()
    );
    assert!(candidates.rgb.len() >= count);

    let center = candidates.bounding_box_center();
    let first = candidates.select_initial_from_center(center);
    let mut output = vec![first.rgb];
    let mut newest = Some(candidates.lab(first.index));

    while output.len() < count {
        let choice = if let Some(center) = newest.take() {
            candidates.update_with_center_and_choose(center)
        } else {
            candidates.farthest_current()
        };
        let center = candidates.lab(choice.index);
        candidates.mark_selected(choice);
        output.push(choice.rgb);
        newest = Some(center);
    }

    eprintln!(
        "{}: generated {} colors in {:.2?}",
        scheme.name,
        output.len(),
        started.elapsed()
    );
    output
}

fn render_generated_file(count: usize) -> String {
    let lookup = LinearLookup::new();
    let mut output = String::new();
    writeln!(
        output,
        "// Generated by `cargo run -p generate-tab-color-palettes --release -- --count {count}`."
    )
    .unwrap();
    writeln!(
        output,
        "// The generator exhaustively ranks eligible 8-bit sRGB colors."
    )
    .unwrap();
    writeln!(output, "// Do not edit by hand.\n").unwrap();

    for (scheme_index, scheme) in SCHEMES.iter().enumerate() {
        if scheme_index > 0 {
            output.push('\n');
        }
        let colors = generate_scheme(*scheme, count, &lookup);
        let mut hasher = Sha256::new();
        for color in &colors {
            hasher.update(color.to_be_bytes());
        }
        let hash = format!("{:x}", hasher.finalize());
        writeln!(output, "// {}", scheme.description()).unwrap();
        writeln!(output, "// RGB sequence SHA-256: {hash}").unwrap();
        writeln!(output, "#[rustfmt::skip]").unwrap();
        writeln!(output, "pub const {}: &[&str] = &[", scheme.constant_name).unwrap();
        for row in colors.chunks(8) {
            output.push_str("    ");
            for (index, color) in row.iter().enumerate() {
                if index > 0 {
                    output.push(' ');
                }
                write!(output, "\"#{color:06x}\",").unwrap();
            }
            output.push('\n');
        }
        output.push_str("];\n");
    }
    output
}

fn srgb_channel_to_linear(value: f32) -> f32 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn oklab_from_linear_rgb(red: f32, green: f32, blue: f32) -> Lab {
    let l = (0.412_221_46 * red + 0.536_332_55 * green + 0.051_445_995 * blue).cbrt();
    let m = (0.211_903_5 * red + 0.680_699_5 * green + 0.107_396_96 * blue).cbrt();
    let s = (0.088_302_46 * red + 0.281_718_85 * green + 0.629_978_7 * blue).cbrt();

    Lab {
        l: 0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        a: 1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_5 * s,
        b: 0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    }
}

struct Args {
    count: usize,
    output: PathBuf,
    check: bool,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut count = DEFAULT_COUNT;
    let mut output = PathBuf::from(DEFAULT_OUTPUT);
    let mut check = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--count" => {
                count = args
                    .next()
                    .context("--count requires a value")?
                    .parse()
                    .context("invalid --count")?;
            }
            "--output" => {
                output = args.next().context("--output requires a value")?.into();
            }
            "--check" => check = true,
            "--help" | "-h" => {
                println!(
                    "usage: generate-tab-color-palettes [--count N] [--output PATH] [--check]"
                );
                std::process::exit(0);
            }
            _ => bail!("unknown argument: {arg}"),
        }
    }
    if count == 0 {
        bail!("--count must be greater than zero");
    }
    Ok(Args {
        count,
        output,
        check,
    })
}

fn write_or_check(path: &Path, generated: &str, check: bool) -> anyhow::Result<()> {
    if check {
        let existing =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        if existing != generated {
            bail!("{} is not up to date", path.display());
        }
        println!("{} is up to date", path.display());
    } else {
        std::fs::write(path, generated).with_context(|| format!("writing {}", path.display()))?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    let generated = render_generated_file(args.count);
    write_or_check(&args.output, &generated, args.check)
}

#[cfg(test)]
mod tests {
    use super::{better_choice, oklab_from_linear_rgb, Candidates, Choice, Lab, LinearLookup};

    #[test]
    fn black_and_white_have_expected_oklab_lightness() {
        assert_eq!(oklab_from_linear_rgb(0.0, 0.0, 0.0).l, 0.0);
        assert!((oklab_from_linear_rgb(1.0, 1.0, 1.0).l - 1.0).abs() < 0.0001);
    }

    #[test]
    fn lookup_uses_actual_inactive_rendering() {
        let lookup = LinearLookup::new();
        let expected = oklab_from_linear_rgb(super::srgb_channel_to_linear(0.4), 0.0, 0.0);
        let actual = lookup.inactive_lab(0xff0000);
        assert!(actual.distance_squared(expected) < f32::EPSILON);
    }

    #[test]
    fn equal_distance_ties_choose_lower_rgb() {
        let left = Choice {
            index: 0,
            rgb: 0x020000,
            distance: 1.0,
        };
        let right = Choice {
            index: 1,
            rgb: 0x010000,
            distance: 1.0,
        };
        assert_eq!(better_choice(left, right).rgb, right.rgb);
    }

    #[test]
    fn distance_is_squared_euclidean_oklab() {
        let a = Lab {
            l: 0.1,
            a: 0.2,
            b: 0.3,
        };
        let b = Lab {
            l: 0.2,
            a: 0.4,
            b: 0.6,
        };
        assert!((a.distance_squared(b) - 0.14).abs() < 0.00001);
    }

    #[test]
    fn farthest_selection_updates_nearest_distance() {
        let mut candidates = Candidates {
            rgb: vec![1, 2, 3],
            l: vec![0.0, 1.0, 3.0],
            a: vec![0.0; 3],
            b: vec![0.0; 3],
            min_distance: vec![f32::INFINITY; 3],
            selected: vec![1, 0, 0],
        };
        let choice = candidates.update_with_center_and_choose(Lab {
            l: 0.0,
            a: 0.0,
            b: 0.0,
        });
        assert_eq!(choice.rgb, 3);
        assert_eq!(choice.distance, 9.0);
        assert_eq!(candidates.min_distance, vec![f32::INFINITY, 1.0, 9.0]);
    }

    #[test]
    fn initial_center_uses_rendered_candidate_bounds() {
        let candidates = Candidates {
            rgb: vec![1, 2],
            l: vec![0.2, 0.6],
            a: vec![-0.4, 0.2],
            b: vec![-0.1, 0.5],
            min_distance: vec![f32::INFINITY; 2],
            selected: vec![0; 2],
        };
        let center = candidates.bounding_box_center();
        assert!((center.l - 0.4).abs() < 0.000001);
        assert!((center.a + 0.1).abs() < 0.000001);
        assert!((center.b - 0.2).abs() < 0.000001);
    }
}
