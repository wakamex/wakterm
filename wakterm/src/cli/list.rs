use crate::cli::CliOutputFormatKind;
use clap::Parser;
use serde::Serializer as _;
use std::io::Write;
use tabout::Alignment;
use termwiz::cell::unicode_column_width;
use unicode_segmentation::UnicodeSegmentation;
use wakterm_client::client::Client;
use wakterm_term::TerminalSize;

const COLUMN_SEPARATOR: &str = " ";
const MAX_PANE_COLUMN_WIDTH: usize = 48;
const MAX_TAB_COLUMN_WIDTH: usize = 24;
const TRUNCATION_MARKER: &str = "...";

#[derive(Debug, Parser, Clone, Copy)]
pub struct ListCommand {
    /// Controls the output format.
    /// "table" and "json" are possible formats.
    #[arg(long = "format", default_value = "table")]
    format: CliOutputFormatKind,
}

impl ListCommand {
    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        let out = std::io::stdout();

        let mut output_items = vec![];
        let panes = client.list_panes().await?;

        for (tabroot, tab_title) in panes.tabs.into_iter().zip(panes.tab_titles.iter()) {
            let mut cursor = tabroot.into_tree().cursor();

            loop {
                if let Some(entry) = cursor.leaf_mut() {
                    let window_title = panes
                        .window_titles
                        .get(&entry.window_id)
                        .map(|s| s.as_str())
                        .unwrap_or("");
                    output_items.push(CliListResultItem::from(
                        entry.clone(),
                        tab_title,
                        window_title,
                    ));
                }
                match cursor.preorder_next() {
                    Ok(c) => cursor = c,
                    Err(_) => break,
                }
            }
        }
        match self.format {
            CliOutputFormatKind::Json => {
                let mut writer = serde_json::Serializer::pretty(out.lock());
                writer.collect_seq(output_items.iter())?;
            }
            CliOutputFormatKind::Table => {
                let rows = output_items
                    .iter()
                    .map(CliListTableRow::from)
                    .collect::<Vec<_>>();
                render_table(&rows, &mut std::io::stdout().lock())?;
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct CliListTableRow {
    window_id: String,
    tab_id: String,
    pane_id: String,
    workspace: String,
    size: String,
    pane: String,
    tab: String,
    cwd: String,
}

impl From<&CliListResultItem> for CliListTableRow {
    fn from(item: &CliListResultItem) -> Self {
        Self {
            window_id: item.window_id.to_string(),
            tab_id: item.tab_id.to_string(),
            pane_id: item.pane_id.to_string(),
            workspace: item.workspace.to_string(),
            size: format!("{}x{}", item.size.cols, item.size.rows),
            pane: truncate_for_table(&item.title, MAX_PANE_COLUMN_WIDTH),
            tab: truncate_for_table(&item.tab_title, MAX_TAB_COLUMN_WIDTH),
            cwd: item.cwd.to_string(),
        }
    }
}

fn truncate_for_table(text: &str, max_width: usize) -> String {
    if unicode_column_width(text, None) <= max_width {
        return text.to_string();
    }

    let marker_width = unicode_column_width(TRUNCATION_MARKER, None);
    if max_width <= marker_width {
        return ".".repeat(max_width);
    }

    let mut result = String::new();
    let mut width = 0;
    let allowed_width = max_width - marker_width;

    for grapheme in UnicodeSegmentation::graphemes(text, true) {
        let grapheme_width = unicode_column_width(grapheme, None);
        if width + grapheme_width > allowed_width {
            break;
        }
        result.push_str(grapheme);
        width += grapheme_width;
    }

    result.push_str(TRUNCATION_MARKER);
    result
}

fn column_width<'a>(header: &str, values: impl Iterator<Item = &'a str>) -> usize {
    values.fold(unicode_column_width(header, None), |width, value| {
        width.max(unicode_column_width(value, None))
    })
}

fn write_padded_column<W: Write>(
    output: &mut W,
    text: &str,
    width: usize,
    alignment: Alignment,
) -> Result<(), std::io::Error> {
    let text_width = unicode_column_width(text, None);
    let padding = width.saturating_sub(text_width);

    if matches!(alignment, Alignment::Right) {
        for _ in 0..padding {
            write!(output, " ")?;
        }
    }

    write!(output, "{text}")?;

    if matches!(alignment, Alignment::Left) {
        for _ in 0..padding {
            write!(output, " ")?;
        }
    }

    Ok(())
}

fn write_row<W: Write>(
    output: &mut W,
    columns: &[(&str, usize, Alignment)],
) -> Result<(), std::io::Error> {
    for (idx, (text, width, alignment)) in columns.iter().enumerate() {
        if idx > 0 {
            write!(output, "{COLUMN_SEPARATOR}")?;
        }
        write_padded_column(output, text, *width, *alignment)?;
    }
    writeln!(output)
}

fn render_table<W: Write>(rows: &[CliListTableRow], output: &mut W) -> Result<(), std::io::Error> {
    let win_width = column_width("WINID", rows.iter().map(|row| row.window_id.as_str()));
    let tabid_width = column_width("TABID", rows.iter().map(|row| row.tab_id.as_str()));
    let paneid_width = column_width("PANEID", rows.iter().map(|row| row.pane_id.as_str()));
    let workspace_width = column_width("WORKSPACE", rows.iter().map(|row| row.workspace.as_str()));
    let size_width = column_width("SIZE", rows.iter().map(|row| row.size.as_str()));
    let pane_width = column_width("PANE", rows.iter().map(|row| row.pane.as_str()));
    let tab_width = column_width("TAB", rows.iter().map(|row| row.tab.as_str()));
    let cwd_width = column_width("CWD", rows.iter().map(|row| row.cwd.as_str()));

    write_row(
        output,
        &[
            ("WINID", win_width, Alignment::Right),
            ("TABID", tabid_width, Alignment::Right),
            ("PANEID", paneid_width, Alignment::Right),
            ("WORKSPACE", workspace_width, Alignment::Left),
            ("SIZE", size_width, Alignment::Right),
            ("PANE", pane_width, Alignment::Left),
            ("TAB", tab_width, Alignment::Left),
            ("CWD", cwd_width, Alignment::Left),
        ],
    )?;

    for row in rows {
        write_row(
            output,
            &[
                (&row.window_id, win_width, Alignment::Right),
                (&row.tab_id, tabid_width, Alignment::Right),
                (&row.pane_id, paneid_width, Alignment::Right),
                (&row.workspace, workspace_width, Alignment::Left),
                (&row.size, size_width, Alignment::Right),
                (&row.pane, pane_width, Alignment::Left),
                (&row.tab, tab_width, Alignment::Left),
                (&row.cwd, cwd_width, Alignment::Left),
            ],
        )?;
    }

    Ok(())
}

#[derive(serde::Serialize)]
struct CliListResultPtySize {
    rows: usize,
    cols: usize,
    /// Pixel width of the pane, if known (can be zero)
    pixel_width: usize,
    /// Pixel height of the pane, if known (can be zero)
    pixel_height: usize,
    /// dpi of the pane, if known (can be zero)
    dpi: u32,
}

// This will be serialized to JSON via the 'List' command.
// As such it is intended to be a stable output format,
// Thus we need to be careful about both the fields and their types,
// herein as they are directly reflected in the output.
#[derive(serde::Serialize)]
struct CliListResultItem {
    window_id: mux::window::WindowId,
    tab_id: mux::tab::TabId,
    pane_id: mux::pane::PaneId,
    workspace: String,
    size: CliListResultPtySize,
    title: String,
    cwd: String,
    /// Cursor x coordinate from top left of non-scrollback pane area
    cursor_x: usize,
    /// Cursor y coordinate from top left of non-scrollback pane area
    cursor_y: usize,
    cursor_shape: termwiz::surface::CursorShape,
    cursor_visibility: termwiz::surface::CursorVisibility,
    /// Number of cols from the left of the tab area to the left of this pane
    left_col: usize,
    /// Number of rows from the top of the tab area to the top of this pane
    top_row: usize,
    tab_title: String,
    window_title: String,
    is_active: bool,
    is_zoomed: bool,
    tty_name: Option<String>,
}

impl CliListResultItem {
    fn from(pane: mux::tab::PaneEntry, tab_title: &str, window_title: &str) -> CliListResultItem {
        let mux::tab::PaneEntry {
            window_id,
            tab_id,
            pane_id,
            workspace,
            title,
            working_dir,
            cursor_pos,
            physical_top,
            left_col,
            top_row,
            is_active_pane,
            is_zoomed_pane,
            tty_name,
            size:
                TerminalSize {
                    rows,
                    cols,
                    pixel_width,
                    pixel_height,
                    dpi,
                },
            ..
        } = pane;

        CliListResultItem {
            window_id,
            tab_id,
            pane_id,
            workspace,
            size: CliListResultPtySize {
                rows,
                cols,
                pixel_width,
                pixel_height,
                dpi,
            },
            title,
            cwd: working_dir
                .as_ref()
                .map(|url| url.url.as_str())
                .unwrap_or("")
                .to_string(),
            cursor_x: cursor_pos.x,
            cursor_y: cursor_pos.y.saturating_sub(physical_top) as usize,
            cursor_shape: cursor_pos.shape,
            cursor_visibility: cursor_pos.visibility,
            left_col,
            top_row,
            tab_title: tab_title.to_string(),
            window_title: window_title.to_string(),
            is_active: is_active_pane,
            is_zoomed: is_zoomed_pane,
            tty_name,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn truncate_for_table_keeps_requested_display_width() {
        let text = "0123456789abcdef";
        let truncated = truncate_for_table(text, 10);

        assert_eq!(unicode_column_width(&truncated, None), 10);
        assert!(truncated.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn render_table_keeps_cwd_aligned_even_with_empty_tab_titles() {
        let rows = vec![
            CliListTableRow {
                window_id: "0".to_string(),
                tab_id: "0".to_string(),
                pane_id: "0".to_string(),
                workspace: "default".to_string(),
                size: "250x68".to_string(),
                pane: "zsh".to_string(),
                tab: "".to_string(),
                cwd: "file://fedora/home/mihai".to_string(),
            },
            CliListTableRow {
                window_id: "0".to_string(),
                tab_id: "2".to_string(),
                pane_id: "2".to_string(),
                workspace: "default".to_string(),
                size: "124x20".to_string(),
                pane: "Claude Code".to_string(),
                tab: "agents".to_string(),
                cwd: "file://fedora/home/mihai".to_string(),
            },
        ];

        let mut output = Vec::new();
        render_table(&rows, &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        let lines = output.lines().collect::<Vec<_>>();

        let first_cwd = lines[1].find("file://").unwrap();
        let second_cwd = lines[2].find("file://").unwrap();

        assert_eq!(first_cwd, second_cwd);
        assert!(lines[0].contains("PANE"));
        assert!(lines[0].contains("TAB"));
    }
}
