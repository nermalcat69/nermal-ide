//! The lines-changed-per-day heatmap in the Info panel — a GitHub
//! contribution graph rotated on its side: one row per month, running top to
//! bottom, rather than one column per week running left to right. Local git
//! history only; a remote repository's commits are not something `git log`
//! here can see.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui::{AnyElement, Context, div, prelude::*, px, rems};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

use crate::ui::app::{CONTENT_INSET, NermalApp};
use crate::ui::i18n::{L10nKey, t};
use crate::ui::right_panel::META;

/// How many months back the graph runs, current month included.
const MONTHS: u32 = 12;

pub(crate) struct HeatmapMonth {
    pub(crate) year: i32,
    pub(crate) month: u32,
    /// One entry per day of the month, index 0 = the 1st: lines added plus
    /// removed across every commit that landed that day.
    pub(crate) days: Vec<u32>,
}

/// Days since the Unix epoch to a proleptic-Gregorian (year, month, day) —
/// Howard Hinnant's `civil_from_days`. The one calendar conversion this
/// heatmap needs, and small enough that pulling in a date crate for it would
/// be the more expensive thing in the file.
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

fn today() -> (i32, u32, u32) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    civil_from_days((secs / 86_400) as i64)
}

fn is_leap_year(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(y) => 29,
        2 => 28,
        _ => 30,
    }
}

/// `MONTHS` calendar months ending on this one, oldest first — the order the
/// rows are drawn in, so the graph reads top-to-bottom as it does left-to-right
/// on GitHub's own.
fn month_window(year: i32, month: u32) -> Vec<(i32, u32)> {
    let mut months = Vec::with_capacity(MONTHS as usize);
    let (mut y, mut m) = (year, month);
    for _ in 0..MONTHS {
        months.push((y, m));
        if m == 1 {
            m = 12;
            y -= 1;
        } else {
            m -= 1;
        }
    }
    months.reverse();
    months
}

/// Runs `git log` for the last `MONTHS` months of history and sums added
/// plus removed lines per day. `None` on anything that is not a readable
/// local git repository — the section simply does not draw rather than
/// showing a graph of zeroes for a directory `git` never heard of.
pub(crate) fn fetch(root: &Path) -> Option<Vec<HeatmapMonth>> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("log")
        .arg(format!("--since={MONTHS} months ago"))
        .arg("--pretty=format:D:%ad")
        .arg("--date=format:%Y-%m-%d")
        .arg("--numstat")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut per_day: HashMap<(i32, u32, u32), u32> = HashMap::new();
    let mut current: Option<(i32, u32, u32)> = None;
    for line in text.lines() {
        if let Some(date) = line.strip_prefix("D:") {
            let parts: Vec<&str> = date.splitn(3, '-').collect();
            current = match parts.as_slice() {
                [y, m, d] => match (y.parse(), m.parse(), d.parse()) {
                    (Ok(y), Ok(m), Ok(d)) => Some((y, m, d)),
                    _ => None,
                },
                _ => None,
            };
            continue;
        }
        let mut fields = line.splitn(3, '\t');
        let added = fields.next().and_then(|s| s.parse::<u32>().ok());
        let removed = fields.next().and_then(|s| s.parse::<u32>().ok());
        if let (Some(added), Some(removed), Some(date)) = (added, removed, current) {
            *per_day.entry(date).or_insert(0) += added + removed;
        }
    }

    let (y, m, _) = today();
    Some(
        month_window(y, m)
            .into_iter()
            .map(|(year, month)| {
                let days = (1..=days_in_month(year, month))
                    .map(|d| per_day.get(&(year, month, d)).copied().unwrap_or(0))
                    .collect();
                HeatmapMonth { year, month, days }
            })
            .collect(),
    )
}

const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Which of five buckets `lines` falls into, the same shape GitHub's own
/// graph uses: none, and four widening bands above it.
fn bucket(lines: u32) -> u8 {
    match lines {
        0 => 0,
        1..=9 => 1,
        10..=49 => 2,
        50..=199 => 3,
        _ => 4,
    }
}

impl NermalApp {
    /// Fetches the heatmap for `root` if the cache is for a different
    /// repository (or there is none yet), then renders whatever is cached —
    /// which is last frame's repository for one frame after a switch, rather
    /// than a blank while the process runs.
    pub(crate) fn heatmap_section(
        &mut self,
        root: Option<&Path>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let root = root?.to_path_buf();
        if self.right_panel.heatmap_root.as_deref() != Some(root.as_path())
            && !self.right_panel.heatmap_loading
        {
            self.right_panel.heatmap_loading = true;
            self.right_panel.heatmap_root = Some(root.clone());
            cx.spawn(async move |this, cx| {
                let months = cx
                    .background_spawn({
                        let root = root.clone();
                        async move { fetch(&root) }
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.right_panel.heatmap_loading = false;
                    if this.right_panel.heatmap_root.as_deref() == Some(root.as_path()) {
                        this.right_panel.heatmap = months;
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        let months = self.right_panel.heatmap.as_ref()?;
        let muted = cx.theme().muted_foreground;
        let success = cx.theme().success;

        let mut rows = v_flex().gap(px(3.));
        for month in months {
            let mut cells = h_flex().gap(px(2.));
            for &lines in &month.days {
                let b = bucket(lines);
                let color = if b == 0 {
                    muted.opacity(0.12)
                } else {
                    success.opacity(0.2 + 0.2 * b as f32)
                };
                cells = cells.child(div().size(px(8.)).rounded(px(2.)).bg(color));
            }
            rows = rows.child(
                h_flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex_none()
                            .w(px(28.))
                            .text_size(rems(META))
                            .text_color(muted)
                            .child(MONTH_NAMES[(month.month - 1) as usize]),
                    )
                    .child(cells),
            );
        }

        Some(
            v_flex()
                .child(self.panel_subtitle(t(L10nKey::PanelActivitySubtitle), true, None, cx))
                .child(
                    div()
                        .id("heatmap-scroll")
                        .px(px(CONTENT_INSET))
                        .py(px(4.))
                        .overflow_x_scroll()
                        .child(rows),
                )
                .into_any_element(),
        )
    }
}
