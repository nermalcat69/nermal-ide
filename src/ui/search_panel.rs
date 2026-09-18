//! Find (and replace) across every file under the workspace's roots —
//! the right panel's Search tab.
//!
//! Local files only for now: it walks `code.roots` with the same
//! gitignore-aware crate the file tree's own walk uses, which only ever
//! makes sense for a path this machine can read directly. A remote
//! workspace's files stay reachable through the tree and the editor; this
//! tab just has nothing to search there yet.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use gpui::{AnyElement, Context, Entity, Window, div, prelude::*, px, rems};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex,
};

use crate::ui::app::{CONTENT_INSET, NermalApp};
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::right_panel::{META, ROW_INSET, TEXT, TEXT_MONO, git_badge};

/// Past this many matches, the search stops looking — a query that matches
/// half the repository is a query nobody is going to read the results of,
/// and an unbounded walk over a large tree would hang the panel instead of
/// answering it.
const MAX_MATCHES: usize = 2000;
/// Past this many *rendered* rows, the list stops drawing more — a plain
/// column of a couple thousand rows is what made scrolling the panel
/// noticeably laggy, and the cap is well past what anyone actually reads
/// through row by row.
const MAX_RENDERED_ROWS: usize = 300;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

pub(crate) struct SearchMatch {
    pub(crate) line: u32,
    pub(crate) text: String,
    /// Byte ranges of every match on this line, for the highlight — a line
    /// found because of the query reads as noise if the query itself does
    /// not stand out from the rest of it.
    pub(crate) ranges: Vec<(usize, usize)>,
}

pub(crate) struct SearchFileResult {
    pub(crate) path: PathBuf,
    pub(crate) matches: Vec<SearchMatch>,
    /// The working-tree status letter (`M`, `A`, `?`, …), when the file's
    /// repository has one — the same letter the file tree and the Source
    /// Control panel wear, so "this file has uncommitted changes" reads the
    /// same way in all three places.
    pub(crate) git_status: Option<char>,
}

#[derive(Default)]
pub(crate) struct SearchPanelState {
    pub(crate) query: Option<Entity<InputState>>,
    pub(crate) replace: Option<Entity<InputState>>,
    pub(crate) case_sensitive: bool,
    pub(crate) whole_word: bool,
    pub(crate) regex: bool,
    /// Restricts the walk to files the repository already reports as
    /// modified — "search only what I have not committed yet".
    pub(crate) modified_only: bool,
    pub(crate) results: Vec<SearchFileResult>,
    pub(crate) file_count: usize,
    pub(crate) match_count: usize,
    pub(crate) searching: bool,
    pub(crate) truncated: bool,
    pub(crate) elapsed_ms: u64,
    /// Files whose match list is folded away. Closing one is bookkeeping,
    /// not data loss, so it is never cleared by a new search unless that
    /// search drops the file it names entirely.
    pub(crate) collapsed: HashSet<PathBuf>,
    /// Bumped on every new search; a search that lands after a newer one
    /// started is a stale answer to a question nobody is asking any more.
    generation: u64,
    _subs: Vec<gpui::Subscription>,
}

/// Builds the one pattern every mode compiles down to: literal text becomes
/// its own escaped regex, so case-sensitivity and whole-word both go through
/// the same matcher instead of a separate substring path that could answer
/// differently.
fn build_pattern(
    query: &str,
    case_sensitive: bool,
    whole_word: bool,
    regex: bool,
) -> Option<regex::Regex> {
    if query.is_empty() {
        return None;
    }
    let body = if regex {
        query.to_string()
    } else {
        regex::escape(query)
    };
    let body = if whole_word {
        format!(r"\b{body}\b")
    } else {
        body
    };
    let pattern = if case_sensitive {
        body
    } else {
        format!("(?i){body}")
    };
    regex::Regex::new(&pattern).ok()
}

impl NermalApp {
    fn search_query_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        if let Some(input) = self.search_panel.query.clone() {
            return input;
        }
        let input = crate::ui::prefill::filled_box(String::new(), window, cx);
        let sub = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::Change | InputEvent::PressEnter { .. }) {
                this.run_search(window, cx);
            }
        });
        self.search_panel.query = Some(input.clone());
        self.search_panel._subs.push(sub);
        input
    }

    fn search_replace_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        if let Some(input) = self.search_panel.replace.clone() {
            return input;
        }
        let input = crate::ui::prefill::filled_box(String::new(), window, cx);
        self.search_panel.replace = Some(input);
        self.search_panel.replace.clone().unwrap()
    }

    /// Kicks off a background walk of every workspace root, and stores the
    /// generation it ran under so a slower, older search cannot overwrite a
    /// faster, newer one landing first.
    fn run_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self
            .search_panel
            .query
            .as_ref()
            .map(|i| i.read(cx).value().to_string())
            .unwrap_or_default();
        self.search_panel.generation += 1;
        let generation = self.search_panel.generation;
        let Some(pattern) = build_pattern(
            &query,
            self.search_panel.case_sensitive,
            self.search_panel.whole_word,
            self.search_panel.regex,
        ) else {
            self.search_panel.results.clear();
            self.search_panel.file_count = 0;
            self.search_panel.match_count = 0;
            self.search_panel.truncated = false;
            self.search_panel.searching = false;
            cx.notify();
            return;
        };
        let roots: Vec<PathBuf> = self
            .code
            .as_ref()
            .map(|c| c.roots.clone())
            .unwrap_or_default();
        // `code.roots` follows the workspace's own host, which is not always
        // this machine — the walk below reads straight off local disk, so a
        // remote workspace's roots have to be held back rather than searched
        // against the wrong filesystem.
        if roots.is_empty() || !self.spawn_host(cx).is_local() {
            self.search_panel.results.clear();
            cx.notify();
            return;
        }
        let modified_only = self.search_panel.modified_only;
        self.search_panel.searching = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let (found, elapsed_ms) = cx
                .background_spawn(async move {
                    let start = std::time::Instant::now();
                    let found = walk_and_search(&roots, &pattern, modified_only);
                    (found, start.elapsed().as_millis() as u64)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.search_panel.generation != generation {
                    return;
                }
                this.search_panel.searching = false;
                this.search_panel.file_count = found.len();
                this.search_panel.match_count = found.iter().map(|f| f.matches.len()).sum();
                this.search_panel.truncated = this.search_panel.match_count >= MAX_MATCHES;
                this.search_panel.elapsed_ms = elapsed_ms;
                let kept: HashSet<PathBuf> = found.iter().map(|f| f.path.clone()).collect();
                this.search_panel.collapsed.retain(|p| kept.contains(p));
                this.search_panel.results = found;
                cx.notify();
            });
        })
        .detach();
    }

    /// Applies the same pattern the results were found with to every one of
    /// them, on disk, in one pass — local files only, the same boundary the
    /// search itself keeps.
    fn search_replace_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self
            .search_panel
            .query
            .as_ref()
            .map(|i| i.read(cx).value().to_string())
            .unwrap_or_default();
        let replacement = self
            .search_panel
            .replace
            .as_ref()
            .map(|i| i.read(cx).value().to_string())
            .unwrap_or_default();
        let Some(pattern) = build_pattern(
            &query,
            self.search_panel.case_sensitive,
            self.search_panel.whole_word,
            self.search_panel.regex,
        ) else {
            return;
        };
        if !self.spawn_host(cx).is_local() {
            return;
        }
        let paths: Vec<PathBuf> = self
            .search_panel
            .results
            .iter()
            .map(|f| f.path.clone())
            .collect();
        cx.spawn_in(window, async move |this, cx| {
            cx.background_spawn(async move {
                for path in &paths {
                    let Ok(content) = std::fs::read_to_string(path) else {
                        continue;
                    };
                    let replaced = pattern.replace_all(&content, replacement.as_str());
                    if replaced != content {
                        let _ = std::fs::write(path, replaced.as_bytes());
                    }
                }
            })
            .await;
            let _ = this.update_in(cx, |this, window, cx| this.run_search(window, cx));
        })
        .detach();
    }

    pub(crate) fn render_panel_search(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title = self.panel_title(t(L10nKey::PanelSearchTitle), None, None, window, cx);
        let query_input = self.search_query_input(window, cx);
        let replace_input = self.search_replace_input(window, cx);

        let flag_tile =
            |label: &'static str, active: bool, tip: &'static str, cx: &Context<Self>| {
                div()
                    .id(label)
                    .flex_none()
                    .px(px(5.))
                    .py(px(2.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .text_size(rems(META))
                    .when(active, |d| {
                        d.bg(cx.theme().primary)
                            .text_color(cx.theme().primary_foreground)
                    })
                    .when(!active, |d| d.text_color(cx.theme().muted_foreground))
                    .child(label)
                    .tooltip(move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(tip).build(window, cx)
                    })
            };

        let query_row = h_flex()
            .items_center()
            .gap(px(4.))
            .px(px(CONTENT_INSET))
            .py(px(4.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&query_input).small()),
            )
            .child(
                flag_tile("Aa", self.search_panel.case_sensitive, "Match Case", cx).on_click(
                    cx.listener(|this, _, window, cx| {
                        this.search_panel.case_sensitive = !this.search_panel.case_sensitive;
                        this.run_search(window, cx);
                    }),
                ),
            )
            .child(
                flag_tile("ab", self.search_panel.whole_word, "Match Whole Word", cx).on_click(
                    cx.listener(|this, _, window, cx| {
                        this.search_panel.whole_word = !this.search_panel.whole_word;
                        this.run_search(window, cx);
                    }),
                ),
            )
            .child(
                flag_tile(".*", self.search_panel.regex, "Use Regular Expression", cx).on_click(
                    cx.listener(|this, _, window, cx| {
                        this.search_panel.regex = !this.search_panel.regex;
                        this.run_search(window, cx);
                    }),
                ),
            )
            .child(
                flag_tile(
                    "M",
                    self.search_panel.modified_only,
                    "Uncommitted Changes Only",
                    cx,
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.search_panel.modified_only = !this.search_panel.modified_only;
                    this.run_search(window, cx);
                })),
            );

        let replace_row = h_flex()
            .items_center()
            .gap(px(4.))
            .px(px(CONTENT_INSET))
            .pb(px(6.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&replace_input).small()),
            )
            .child(
                Button::new("search-replace-all")
                    .label(t(L10nKey::PanelReplaceAll))
                    .small()
                    .disabled(self.search_panel.results.is_empty())
                    .on_click(
                        cx.listener(|this, _, window, cx| this.search_replace_all(window, cx)),
                    ),
            );

        let summary = div()
            .px(px(CONTENT_INSET))
            .pb(px(6.))
            .text_size(rems(META))
            .text_color(cx.theme().muted_foreground)
            .child(if self.search_panel.searching {
                t(L10nKey::PanelSearching).to_string()
            } else if self.search_panel.results.is_empty() {
                String::new()
            } else {
                let base = t_fmt(
                    L10nKey::PanelSearchSummary,
                    &[
                        ("matches", &self.search_panel.match_count.to_string()),
                        ("files", &self.search_panel.file_count.to_string()),
                    ],
                );
                let mut line = format!("{base} ({} ms)", self.search_panel.elapsed_ms);
                if self.search_panel.truncated {
                    line = format!("{line} — {}", t(L10nKey::PanelSearchTruncated));
                }
                line
            });

        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let mono = cx.theme().mono_font_family.clone();
        let mut list = v_flex().px(px(CONTENT_INSET - ROW_INSET)).py(px(2.));
        let mut rendered = 0usize;
        'files: for (fi, file) in self.search_panel.results.iter().enumerate() {
            let name = file
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| file.path.display().to_string());
            let collapsed = self.search_panel.collapsed.contains(&file.path);
            let path_for_toggle = file.path.clone();
            list =
                list.child(
                    h_flex()
                        .id(("search-file", fi))
                        .cursor_pointer()
                        .items_center()
                        .gap(px(6.))
                        .px(px(ROW_INSET))
                        .py(px(3.))
                        .rounded(px(4.))
                        .hover(|s| s.bg(gpui::rgb(sf.hover)))
                        .child(
                            Icon::new(if collapsed {
                                IconName::ChevronRight
                            } else {
                                IconName::ChevronDown
                            })
                            .size(px(11.))
                            .text_color(cx.theme().muted_foreground),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(rems(TEXT))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .when(file.git_status.is_some(), |d| {
                                    d.text_color(cx.theme().success)
                                })
                                .child(name),
                        )
                        .children(file.git_status.map(|letter| {
                            git_badge(&letter.to_string(), cx.theme().success, &mono)
                        }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.search_panel.collapsed.remove(&path_for_toggle) {
                                this.search_panel.collapsed.insert(path_for_toggle.clone());
                            }
                            cx.notify();
                        })),
                );
            if collapsed {
                continue;
            }
            for (mi, m) in file.matches.iter().enumerate() {
                if rendered >= MAX_RENDERED_ROWS {
                    break 'files;
                }
                rendered += 1;
                let path = file.path.clone();
                let line = m.line;
                let mut text_row = h_flex()
                    .flex_1()
                    .min_w_0()
                    .text_size(rems(TEXT_MONO))
                    .font_family(mono.clone());
                let trimmed_offset = m.text.len() - m.text.trim_start().len();
                let trimmed = m.text.trim();
                let mut cursor = trimmed_offset;
                for &(start, end) in &m.ranges {
                    if start < trimmed_offset {
                        continue;
                    }
                    if cursor < start {
                        text_row = text_row.child(
                            div().child(m.text.get(cursor..start).unwrap_or_default().to_string()),
                        );
                    }
                    text_row = text_row.child(
                        div()
                            .rounded(px(2.))
                            .bg(cx.theme().warning.opacity(0.35))
                            .child(m.text.get(start..end).unwrap_or_default().to_string()),
                    );
                    cursor = end;
                }
                if cursor < m.text.len() {
                    text_row = text_row.child(
                        div()
                            .truncate()
                            .child(m.text.get(cursor..).unwrap_or_default().to_string()),
                    );
                }
                let _ = trimmed;
                list = list.child(
                    h_flex()
                        .id(("search-match", fi * 10_000 + mi))
                        .cursor_pointer()
                        .gap(px(8.))
                        .px(px(ROW_INSET + 10.))
                        .py(px(1.))
                        .rounded(px(4.))
                        .hover(|s| s.bg(gpui::rgb(sf.hover)))
                        .child(
                            div()
                                .flex_none()
                                .text_size(rems(META))
                                .font_family(mono.clone())
                                .text_color(cx.theme().muted_foreground)
                                .child(line.to_string()),
                        )
                        .child(text_row)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_file_in_editor_at(&path, Some(line), None, window, cx);
                        })),
                );
            }
        }
        if rendered >= MAX_RENDERED_ROWS && self.search_panel.match_count > rendered {
            list = list.child(
                div()
                    .px(px(ROW_INSET))
                    .py(px(4.))
                    .text_size(rems(META))
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "Showing the first {rendered} results — narrow the search to see the rest."
                    )),
            );
        }

        let body = if self.search_panel.results.is_empty() && !self.search_panel.searching {
            self.panel_empty(t(L10nKey::PanelSearchEmpty), None, cx)
        } else {
            list.into_any_element()
        };

        v_flex()
            .flex_1()
            .min_h_0()
            .child(title)
            .child(query_row)
            .child(replace_row)
            .child(summary)
            .child(self.panel_scroll(body, div().into_any_element()))
            .into_any_element()
    }
}

/// The git status letter for every modified path under `root`, and the set
/// itself when `modified_only` wants the walk restricted to just those
/// files. `git status --porcelain=v1` is asked directly rather than through
/// the app's own status cache: that cache is keyed by pane and pane
/// terminals do not always line up one-to-one with search roots.
fn git_status_map(root: &Path) -> std::collections::HashMap<PathBuf, char> {
    let mut map = std::collections::HashMap::new();
    let Ok(output) = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("status")
        .arg("--porcelain=v1")
        .arg("--no-renames")
        .output()
    else {
        return map;
    };
    if !output.status.success() {
        return map;
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.len() <= 3 {
            continue;
        }
        let (code, rel) = line.split_at(2);
        let letter = code.trim().chars().next().unwrap_or('M');
        map.insert(root.join(rel.trim()), letter);
    }
    map
}

/// The walk itself: gitignore-aware, skipping anything that does not look
/// like text, capped at [`MAX_MATCHES`] total so one huge match never turns
/// the search into a hang.
fn walk_and_search(
    roots: &[PathBuf],
    pattern: &regex::Regex,
    modified_only: bool,
) -> Vec<SearchFileResult> {
    let mut results = Vec::new();
    let mut total = 0usize;
    'roots: for root in roots {
        let git_status = git_status_map(root);
        if modified_only && git_status.is_empty() {
            continue;
        }
        let mut builder = ignore::WalkBuilder::new(root);
        builder.hidden(false);
        for entry in builder.build().filter_map(Result::ok) {
            if total >= MAX_MATCHES {
                break 'roots;
            }
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            let status = git_status.get(path).copied();
            if modified_only && status.is_none() {
                continue;
            }
            if entry.metadata().is_ok_and(|m| m.len() > MAX_FILE_BYTES) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(path) else {
                continue;
            };
            let mut matches = Vec::new();
            for (ix, line) in content.lines().enumerate() {
                let ranges: Vec<(usize, usize)> = pattern
                    .find_iter(line)
                    .map(|m| (m.start(), m.end()))
                    .collect();
                if !ranges.is_empty() {
                    matches.push(SearchMatch {
                        line: (ix + 1) as u32,
                        text: line.to_string(),
                        ranges,
                    });
                    total += 1;
                    if total >= MAX_MATCHES {
                        break;
                    }
                }
            }
            if !matches.is_empty() {
                results.push(SearchFileResult {
                    path: path.to_path_buf(),
                    matches,
                    git_status: status,
                });
            }
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_insensitive_is_the_default() {
        let p = build_pattern("Foo", false, false, false).unwrap();
        assert!(p.is_match("a foo b"));
        assert!(p.is_match("a FOO b"));
    }

    #[test]
    fn case_sensitive_only_matches_the_case_typed() {
        let p = build_pattern("Foo", true, false, false).unwrap();
        assert!(p.is_match("Foo"));
        assert!(!p.is_match("foo"));
    }

    #[test]
    fn whole_word_does_not_match_inside_another_word() {
        let p = build_pattern("cat", false, true, false).unwrap();
        assert!(p.is_match("a cat sat"));
        assert!(!p.is_match("concatenate"));
    }

    #[test]
    fn a_literal_query_does_not_run_as_a_pattern() {
        // Not whole-word or regex: `.` must match a literal dot, not "any character".
        let p = build_pattern("a.b", false, false, false).unwrap();
        assert!(p.is_match("a.b"));
        assert!(!p.is_match("axb"));
    }

    #[test]
    fn regex_mode_runs_the_query_as_written() {
        let p = build_pattern(r"\d+", false, false, true).unwrap();
        assert!(p.is_match("room 42"));
        assert!(!p.is_match("no digits here"));
    }

    #[test]
    fn a_blank_query_builds_no_pattern() {
        assert!(build_pattern("", false, false, false).is_none());
    }
}
