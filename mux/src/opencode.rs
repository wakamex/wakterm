//! Reads OpenCode's input box from its screen.
//!
//! OpenCode draws its prompt as rows that start with a `┃` bar on the box's
//! own background, ending in a mode line, such as `┃  Build auto · <model>`,
//! right above the box's `╹▀▀▀` bottom border. In an empty box it draws a
//! placeholder, `Ask anything… "<example>"` or, in shell mode,
//! `Run a command… "<example>"`, in its theme's muted color, the color of
//! the mode line's `·` separators. Typed text is drawn in another color.

use wakterm_term::color::ColorAttribute;
use wakterm_term::Line;

const BAR: &str = "\u{2503}";
const BOTTOM: char = '\u{2579}';
const SEPARATOR: &str = "\u{b7}";
const PLACEHOLDERS: [&str; 2] = ["Ask anything\u{2026}", "Run a command\u{2026}"];

/// The background just right of the row's `┃` bar, which is the box's own.
fn bar_background(line: &Line) -> Option<ColorAttribute> {
    let mut cells = line.visible_cells().skip_while(|cell| cell.str() != BAR);
    cells.next()?;
    Some(cells.next()?.attrs().background())
}

/// The text typed into OpenCode's input box, empty when it holds only its
/// placeholder, or None when the box is not on screen.
pub fn input_box(lines: &[Line]) -> Option<String> {
    let plain: Vec<String> = lines.iter().map(|line| line.as_str().to_string()).collect();
    let mode_row = (0..plain.len().saturating_sub(1)).rev().find(|&i| {
        plain[i].trim_start().starts_with(BAR) && plain[i + 1].trim_start().starts_with(BOTTOM)
    })?;
    let background = bar_background(&lines[mode_row])?;
    let muted = lines[mode_row]
        .visible_cells()
        .find(|cell| cell.str() == SEPARATOR)
        .map(|cell| cell.attrs().foreground());
    let first_row = (0..mode_row)
        .rev()
        .take_while(|&i| {
            plain[i].trim_start().starts_with(BAR) && bar_background(&lines[i]) == Some(background)
        })
        .last()
        .unwrap_or(mode_row);
    let rows: Vec<String> = lines[first_row..mode_row]
        .iter()
        .map(|line| {
            line.visible_cells()
                .skip_while(|cell| cell.str() != BAR)
                .skip(1)
                .filter(|cell| Some(cell.attrs().foreground()) != muted)
                .map(|cell| cell.str().to_string())
                .collect::<String>()
                .trim()
                .to_string()
        })
        .filter(|row| !row.is_empty())
        .collect();
    let text = rows.join("\n");
    // Without a separator to learn the muted color from, the placeholder
    // is known by its words.
    if muted.is_none() && PLACEHOLDERS.iter().any(|words| text.starts_with(words)) {
        return Some(String::new());
    }
    Some(text)
}

#[cfg(test)]
mod test {
    use super::*;
    use wakterm_term::color::SrgbaTuple;
    use wakterm_term::{Cell, CellAttributes};

    fn color(r: u8, g: u8, b: u8) -> ColorAttribute {
        ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple::from((r, g, b, 255)))
    }

    /// A screen row: text runs, each with its foreground and background.
    fn row(runs: &[(&str, ColorAttribute, ColorAttribute)]) -> Line {
        let mut cells = Vec::new();
        for (text, fg, bg) in runs {
            let mut attrs = CellAttributes::default();
            attrs.set_foreground(*fg);
            attrs.set_background(*bg);
            cells.extend(text.chars().map(|c| Cell::new(c, attrs.clone())));
        }
        Line::from_cells(cells, 0)
    }

    #[test]
    fn the_box_holds_typed_text_and_not_its_placeholder() {
        let (page, boxed) = (color(10, 10, 10), color(30, 30, 30));
        let (accent, text, muted) = (
            color(92, 156, 245),
            color(255, 255, 255),
            color(128, 128, 128),
        );
        let screen = |typed: &[(&str, ColorAttribute)]| {
            let mut box_row = vec![("  ", text, page), (BAR, accent, page), ("  ", text, boxed)];
            box_row.extend(typed.iter().map(|(words, fg)| (*words, *fg, boxed)));
            vec![
                // A user message in the history also has a bar, on the page.
                row(&[
                    ("  ", text, page),
                    (BAR, accent, page),
                    ("  earlier message", text, page),
                ]),
                row(&[("     \u{25a3}  Build \u{b7} model \u{b7} 3.5s", muted, page)]),
                row(&[
                    ("  ", text, page),
                    (BAR, accent, page),
                    ("   ", text, boxed),
                ]),
                row(&box_row),
                row(&[
                    ("  ", text, page),
                    (BAR, accent, page),
                    ("  ", text, boxed),
                    ("Build", accent, boxed),
                    (" ", text, boxed),
                    ("auto", muted, boxed),
                    (" \u{b7} ", muted, boxed),
                    ("DeepSeek V4.1 Flash", text, boxed),
                ]),
                row(&[("  \u{2579}\u{2580}\u{2580}\u{2580}", accent, page)]),
                row(&[("   /code/gta6", muted, page)]),
            ]
        };
        assert_eq!(input_box(&screen(&[])).as_deref(), Some(""));
        assert_eq!(
            input_box(&screen(&[("Ask anything\u{2026} \"Fix a TODO\"", muted)])).as_deref(),
            Some("")
        );
        assert_eq!(
            input_box(&screen(&[("half a thought", text)])).as_deref(),
            Some("half a thought")
        );
        // A screen without OpenCode's box.
        assert_eq!(input_box(&[row(&[("$ ls", text, page)])]), None);
    }
}
