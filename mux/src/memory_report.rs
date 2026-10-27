//! Periodic memory reporting for the mux server.
//!
//! Logs process RSS and swap, the allocator's heap, and per-pane metrics
//! (scrollback size, action buffer depth) to help diagnose memory growth in
//! long-running sessions.
//!
//! With glibc, every report also returns free heap pages to the system with
//! `malloc_trim`. glibc gives threads their own arenas, up to eight per CPU,
//! and keeps the pages they free: on 2026-10-27 the server's 256 arenas held
//! 4.3 GB with 325 MB in use, and one `malloc_trim` returned 2.4 GB.

use crate::pane::PaneId;
use crate::Mux;

/// Read RSS from /proc/self/statm (Linux only).
/// Returns (rss_bytes, None) on success, or (0, Some(error)) on failure.
fn read_rss() -> usize {
    #[cfg(target_os = "linux")]
    {
        if let Ok(statm) = std::fs::read_to_string("/proc/self/statm") {
            // Fields: size resident shared text lib data dt (in pages)
            if let Some(rss_pages) = statm.split_whitespace().nth(1) {
                if let Ok(pages) = rss_pages.parse::<usize>() {
                    return pages * page_size();
                }
            }
        }
        0
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// The process's swapped-out memory, from /proc/self/status.
fn read_swap() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("VmSwap:"))
                .and_then(|value| {
                    value
                        .trim()
                        .trim_end_matches("kB")
                        .trim()
                        .parse::<usize>()
                        .ok()
                })
        })
        .map_or(0, |kib| kib * 1024)
}

/// glibc's heap: bytes in use, free bytes it keeps, and bytes in mmapped
/// chunks, summed over every arena; then the RSS and swap that
/// `malloc_trim` returned. None with another allocator.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn heap_report() -> Option<String> {
    // SAFETY: mallinfo2 and malloc_trim take no pointers and lock the
    // arenas they read.
    let info = unsafe { libc::mallinfo2() };
    let before = read_rss() + read_swap();
    unsafe { libc::malloc_trim(0) };
    let returned = before.saturating_sub(read_rss() + read_swap());
    Some(format!(
        "heap {} in use, {} free, {} mmapped, trim returned {}",
        format_bytes(info.uordblks),
        format_bytes(info.fordblks),
        format_bytes(info.hblkhd),
        format_bytes(returned)
    ))
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn heap_report() -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn page_size() -> usize {
    // SAFETY: sysconf is always safe to call with _SC_PAGESIZE
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

fn format_bytes(bytes: usize) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}G", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1}M", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0}K", bytes as f64 / 1024.0)
    } else {
        format!("{}B", bytes)
    }
}

/// Per-pane memory info.
struct PaneMemInfo {
    scrollback_rows: usize,
    /// Bytes parsed since last flush in the action buffer.
    action_buf_bytes: usize,
}

/// Collect per-pane memory metrics and log a summary.
pub fn log_memory_report() {
    let rss = read_rss();
    let mux = match Mux::try_get() {
        Some(m) => m,
        None => return,
    };

    let panes = mux.iter_panes();
    let action_sizes = crate::ACTION_BUFFER_SIZES.read();

    let mut infos: Vec<(PaneId, PaneMemInfo)> = Vec::with_capacity(panes.len());
    let mut total_scrollback: usize = 0;
    let mut total_action_bytes: usize = 0;

    for pane in &panes {
        let id = pane.pane_id();
        let dims = pane.get_dimensions();
        let action_buf_bytes = action_sizes
            .get(&id)
            .map(|a| a.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0);

        total_scrollback += dims.scrollback_rows;
        total_action_bytes += action_buf_bytes;

        infos.push((
            id,
            PaneMemInfo {
                scrollback_rows: dims.scrollback_rows,
                action_buf_bytes,
            },
        ));
    }

    drop(action_sizes);

    // Build per-pane detail for any pane with a non-trivial action buffer
    let mut pane_details = String::new();
    for (id, info) in &infos {
        if info.action_buf_bytes > 0 {
            if !pane_details.is_empty() {
                pane_details.push_str(", ");
            }
            pane_details.push_str(&format!(
                "pane {}: {} scrollback, {} buffered",
                id,
                info.scrollback_rows,
                format_bytes(info.action_buf_bytes)
            ));
        }
    }

    if rss == 0 && panes.is_empty() {
        return;
    }

    let heap = heap_report().map_or(String::new(), |heap| format!(" | {heap}"));
    log::info!(
        "memory: RSS {}, swap {}{heap} | {} panes, {} total scrollback rows, {} buffered{}",
        format_bytes(rss),
        format_bytes(read_swap()),
        panes.len(),
        total_scrollback,
        format_bytes(total_action_bytes),
        if pane_details.is_empty() {
            String::new()
        } else {
            format!(" | {}", pane_details)
        }
    );
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    fn heap_report_reads_glibc_and_trims() {
        // Free a few megabytes the allocator could keep.
        drop((0..64).map(|_| vec![1u8; 64 * 1024]).collect::<Vec<_>>());
        let report = heap_report().unwrap();
        assert!(report.starts_with("heap "), "{report}");
        assert!(report.contains(" in use, ") && report.contains("trim returned"), "{report}");
        assert!(read_rss() > 0);
    }
}
