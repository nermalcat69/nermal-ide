//! A terminal popped out of its workspace into a plain window of its own —
//! see `NermalApp::pop_out_terminal`. Closing the window is the way back:
//! `on_window_should_close` hands the same pane to
//! `NermalApp::reattach_popped_terminal` before letting the close proceed, so
//! the terminal (still running whatever it was running) lands as a new tab
//! on the workspace it left rather than being torn down with the window.

use gpui::{
    App, AppContext as _, Bounds, Context, InteractiveElement as _, IntoElement, ParentElement,
    Render, SharedString, Styled, WeakEntity, Window, WindowBounds, WindowOptions, div,
    prelude::FluentBuilder as _, px, size,
};
use gpui_component::{ActiveTheme as _, Root, TitleBar, v_flex};

use crate::ui::app::NermalApp;
use crate::ui::i18n::{L10nKey, t};
use crate::ui::pane::PaneSlot;

const DEFAULT_SIZE: (f32, f32) = (760., 480.);

pub(crate) struct FloatingTerminal {
    slot: PaneSlot,
    origin: WeakEntity<NermalApp>,
}

impl FloatingTerminal {
    fn title(&self, cx: &App) -> SharedString {
        self.slot
            .terminal()
            .and_then(|view| view.read(cx).effective_cwd())
            .and_then(|cwd| cwd.file_name().map(|n| n.to_string_lossy().into_owned()))
            .map(SharedString::from)
            .unwrap_or_else(|| SharedString::from(t(L10nKey::CmdGroupTerminal)))
    }
}

impl Render for FloatingTerminal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self.title(cx);
        v_flex()
            .id("floating-terminal")
            .size_full()
            .bg(cx.theme().background)
            .child(TitleBar::new().child(div().px_2().text_sm().child(title)))
            .child(
                div()
                    .id("floating-terminal-body")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .when_some(self.slot.terminal().cloned(), |el, view| el.child(view)),
            )
    }
}

/// Gives `slot` a window of its own. `origin` is the workspace the pane came
/// from — the only thing the window remembers about where it belongs, and
/// the only thing `on_window_should_close` needs to hand the pane back to.
pub(crate) fn open(origin: WeakEntity<NermalApp>, slot: PaneSlot, cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(DEFAULT_SIZE.0), px(DEFAULT_SIZE.1)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        ..Default::default()
    };
    let opened = cx.open_window(options, {
        let slot = slot.clone();
        let origin = origin.clone();
        move |window, cx| {
            let view = cx.new(|_| FloatingTerminal { slot, origin });
            window.on_window_should_close(cx, {
                let view = view.clone();
                move |_window, cx| {
                    let slot = view.read(cx).slot.clone();
                    let origin = view.read(cx).origin.clone();
                    let _ = origin.update(cx, |app, cx| app.reattach_popped_terminal(slot, cx));
                    true
                }
            });
            cx.new(|cx| Root::new(view, window, cx))
        }
    });
    if opened.is_err() {
        // No window to hand the pane to — better it stay where it was than
        // vanish. The caller already removed it from the tree, so this puts
        // it straight back rather than leaving it orphaned.
        let _ = origin.update(cx, |app, cx| app.reattach_popped_terminal(slot, cx));
    }
}
