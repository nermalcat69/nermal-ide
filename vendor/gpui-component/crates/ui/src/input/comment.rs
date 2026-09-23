use gpui::{Context, Window};

use crate::input::{InputState, RopeExt as _, ToggleComment, mode::InputMode};

/// The line-comment token for a code editor language, `None` for languages
/// that only have block comments (HTML, CSS, Markdown, templates).
fn line_comment(language: &str) -> Option<&'static str> {
    Some(match language {
        "rust" | "go" | "javascript" | "typescript" | "tsx" | "json" | "c" | "cpp" | "java"
        | "kotlin" | "swift" | "scala" | "zig" | "proto" | "csharp" | "php" => "//",
        "python" | "bash" | "toml" | "yaml" | "ruby" | "make" | "cmake" | "elixir"
        | "graphql" => "#",
        "lua" | "sql" => "--",
        _ => return None,
    })
}

/// Comments every non-blank line at their shared indent, or uncomments them
/// all when every one already is — the same toggle rule VS Code and Zed use.
fn toggle_lines(text: &str, token: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let code = lines.iter().filter(|l| !l.trim().is_empty());
    let uncomment = code.clone().all(|l| l.trim_start().starts_with(token));
    let indent = code
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                l.to_string()
            } else if uncomment {
                let at = l.len() - l.trim_start().len();
                let rest = &l[at + token.len()..];
                format!("{}{}", &l[..at], rest.strip_prefix(' ').unwrap_or(rest))
            } else {
                format!("{}{token} {}", &l[..indent], &l[indent..])
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl InputState {
    pub(super) fn toggle_comment(
        &mut self,
        _: &ToggleComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let InputMode::CodeEditor { language, .. } = &self.mode else {
            cx.propagate();
            return;
        };
        let Some(token) = line_comment(language) else {
            return;
        };
        let sel = self.selected_range;
        let (lo, hi) = (sel.start.min(sel.end), sel.start.max(sel.end));
        let first = self.text.offset_to_point(lo).row;
        let mut last = self.text.offset_to_point(hi).row;
        // A selection dragged down to the start of the next line doesn't
        // include that line.
        if hi > lo && last > first && self.text.offset_to_point(hi).column == 0 {
            last -= 1;
        }
        let start = self.text.line_start_offset(first);
        let end = self.text.line_end_offset(last);
        let old = self.text.slice(start..end).to_string();
        let new = toggle_lines(&old, token);
        // One replace, so the whole toggle is one undo step.
        self.replace_text_in_range_silent(
            Some(self.range_to_utf16(&(start..end))),
            &new,
            window,
            cx,
        );
        self.selected_range = if hi > lo {
            (start..start + new.len()).into()
        } else {
            let caret = (lo as isize + new.len() as isize - old.len() as isize)
                .clamp(start as isize, (start + new.len()) as isize);
            (caret as usize..caret as usize).into()
        };
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::toggle_lines;

    #[test]
    fn toggles_at_shared_indent_and_back() {
        let on = toggle_lines("    let x = 1;\n\n        y();", "//");
        assert_eq!(on, "    // let x = 1;\n\n    //     y();");
        assert_eq!(toggle_lines(&on, "//"), "    let x = 1;\n\n        y();");
        // Mixed: one line already commented, so the block gets commented.
        assert_eq!(toggle_lines("# a\nb", "#"), "# # a\n# b");
        assert_eq!(toggle_lines("#a", "#"), "a");
    }
}
