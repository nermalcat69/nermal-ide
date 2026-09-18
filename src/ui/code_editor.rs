use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, Entity, Focusable as _, PromptLevel, SharedString, Subscription, Window,
    div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState, Position, TabSize};
use gpui_component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex,
};

use crate::ui::app::NermalApp;
use crate::ui::document_column::DocumentChrome;
use crate::ui::host_ops::{HostId, HostOps, MTime, SharedHost, WatchSub};
use crate::ui::i18n::{L10nKey, t, t_fmt};

const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

const RELOAD_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

/// How long auto-save waits after the last keystroke before writing. Long
/// enough that a fast typist does not spawn a save per character, short
/// enough that switching files or closing the tab rarely beats it there.
const AUTO_SAVE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(800);

pub(crate) struct OpenFile {
    pub(crate) path: PathBuf,
    /// The machine `path` lives on, held rather than looked up. Saves,
    /// reloads and duplicate detection all key on its id — an SFTP file and a
    /// local file can share the string `/etc/hosts` without being the same
    /// file — and saving goes straight through this handle, so a buffer stays
    /// saveable however the window's own machine has changed underneath it.
    pub(crate) host: SharedHost,
    pub(crate) input: Entity<InputState>,
    pub(crate) dirty: bool,
    disk_mtime: Option<MTime>,
    edit_seq: u64,
    saving: Option<u64>,
    save_pending: bool,
    save_then_close: bool,
    reload_seq: u64,
    pub(crate) conflict: bool,
    pub(crate) preview: bool,
    pub(crate) wrap: bool,
    /// The rendered-Markdown pane's own scroll. Per file, so switching away
    /// and back lands where you were reading — and so the pane can carry the
    /// scrollbar every other scrolling surface in nermal has. The editor itself
    /// gets one from `Input`.
    pub(crate) preview_scroll: gpui::ScrollHandle,
    _sub: Subscription,
    _observe: Subscription,
}

impl OpenFile {
    fn label(&self) -> SharedString {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.display().to_string())
            .into()
    }
}

pub(crate) struct TabCode {
    pub(crate) visible: bool,
    pub(crate) files: Vec<OpenFile>,
    pub(crate) active: usize,
    pub(crate) roots: Vec<PathBuf>,
    /// `roots` came from a real folder (a pane's, or "Open Folder"), so it is
    /// the workspace's and stops following panes. See `file_tree_refresh_roots`.
    pub(crate) rooted: bool,
    /// The folder the machine last said this workspace has, once applied
    /// here — so a render does not re-apply it, and so a folder found from a
    /// pane before the machine answered still gets written back.
    pub(crate) synced_folder: Option<PathBuf>,
    /// Folders added by hand via "Open Folder…", kept apart from `roots`
    /// because [`crate::ui::app::NermalApp::file_tree_refresh_roots`]
    /// recomputes `roots` from scratch off the active pane's cwd every time a
    /// pane changes — a folder only living in `roots` would vanish the next
    /// time that ran. These are folded back in on every refresh instead.
    pub(crate) pinned_roots: Vec<PathBuf>,
    pub(crate) expanded: std::collections::HashSet<PathBuf>,
    pub(crate) selected: Option<PathBuf>,
}

impl TabCode {
    pub(crate) fn new() -> Self {
        Self {
            visible: false,
            files: Vec::new(),
            active: 0,
            roots: Vec::new(),
            rooted: false,
            synced_folder: None,
            pinned_roots: Vec::new(),
            expanded: std::collections::HashSet::new(),
            selected: None,
        }
    }

    pub(crate) fn active_file(&self) -> Option<&OpenFile> {
        self.files.get(self.active)
    }
}

pub(crate) struct EditorPanelState {
    /// Where to put the cursor once a particular file is on screen, for a
    /// `file.rs:120:3` that has to load first. Carried rather than applied at
    /// the call site because opening is asynchronous: the click is long over
    /// by the time there is a buffer to put a cursor in.
    pending_cursor: Option<(PathBuf, u32, u32)>,
    watch: Option<Arc<WatchSub>>,
    watch_host: Option<SharedHost>,
    watch_opening: bool,
    watch_busy: bool,
    watch_dirty: bool,
    watched_dirs: HashSet<PathBuf>,
    watched_files: HashSet<PathBuf>,
    events_tx: smol::channel::Sender<Vec<PathBuf>>,
}

impl EditorPanelState {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<NermalApp>) -> Self {
        let (tx, rx) = smol::channel::unbounded::<Vec<PathBuf>>();
        cx.spawn_in(window, async move |app, cx| {
            while let Ok(first) = rx.recv().await {
                cx.background_executor().timer(RELOAD_DEBOUNCE).await;
                let mut changed: HashSet<PathBuf> = first.into_iter().collect();
                while let Ok(more) = rx.try_recv() {
                    changed.extend(more);
                }
                let ok = app.update_in(cx, |app, window, cx| {
                    for path in changed {
                        if app.editor.watched_files.contains(&path) {
                            app.editor_handle_external_change(&path, window, cx);
                        }
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();
        Self {
            pending_cursor: None,
            watch: None,
            watch_host: None,
            watch_opening: false,
            watch_busy: false,
            watch_dirty: false,
            watched_dirs: HashSet::new(),
            watched_files: HashSet::new(),
            events_tx: tx,
        }
    }
}

pub(crate) fn language_for_path(path: &Path) -> &'static str {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        let lowered = name.to_ascii_lowercase();
        match lowered.as_str() {
            "makefile" | "gnumakefile" => return "make",
            "cmakelists.txt" => return "cmake",
            _ => {}
        }
        if lowered.starts_with('.') && (lowered.contains("shrc") || lowered.ends_with("profile")) {
            return "bash";
        }
    }
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return "text";
    };
    match ext.to_ascii_lowercase().as_str() {
        "rs" => "rust",
        "go" => "go",
        "py" | "pyi" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "json" | "jsonc" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "html" | "htm" => "html",
        "css" => "css",
        "md" | "markdown" => "markdown",
        "sh" | "bash" | "zsh" => "bash",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "cpp",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "lua" => "lua",
        "rb" => "ruby",
        "php" => "php",
        "sql" => "sql",
        "swift" => "swift",
        "scala" => "scala",
        "zig" => "zig",
        "proto" => "proto",
        "diff" | "patch" => "diff",
        "ex" | "exs" => "elixir",
        "erb" => "erb",
        "ejs" => "ejs",
        "svelte" => "svelte",
        "astro" => "astro",
        "graphql" | "gql" => "graphql",
        "cs" => "csharp",
        "cmake" => "cmake",
        _ => "text",
    }
}

/// How many frames a jump-to-line may wait for the editor to be laid out.
///
/// `InputState::scroll_to` gives up when the buffer has never been painted,
/// and that is exactly the state a file that just opened is in: the cursor
/// lands on the right line and the view stays at the top of the file, which
/// is the one thing a `:120` link exists to avoid. Three frames is the same
/// bounded-retry shape `prefill::select_all_when_drawn` uses against the same
/// class of problem.
const CURSOR_SCROLL_ATTEMPTS: u8 = 3;

/// Puts the cursor at `position`, re-trying on later frames until the scroll
/// that follows it can actually be computed.
///
/// Applied immediately as well as on the retry: the cursor itself lands with
/// no layout, so the position is right even for a pane that never paints.
fn place_cursor(
    input: Entity<InputState>,
    position: Position,
    left: u8,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    input.update(cx, |state, cx| {
        state.set_cursor_position(position, window, cx);
    });
    // A target near the top of the file scrolls nowhere and is already done;
    // so is one that has landed. Either way this stops.
    if left <= 1 || input.read(cx).scroll_offset().y != px(0.) {
        return;
    }
    window.on_next_frame(move |window, cx| {
        place_cursor(input, position, left - 1, window, cx);
    });
    // Registering a callback does not by itself ask for a frame, and an
    // overlay that has finished drawing has no other reason to produce one.
    window.refresh();
}

/// Why the built-in editor could not take a file.
enum EditorOpenError {
    /// Not text, so the editor was never the right place for it.
    NotText(PathBuf),
    /// Something already worded for the user.
    Message(String),
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

/// Whether handing this path to the desktop would run it rather than show it.
///
/// The execute bit is what `open` reads to decide between displaying a file
/// and launching it; Windows has no such bit, so there the extension is the
/// only thing that says so.
fn is_program(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return std::fs::metadata(path)
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    }
    #[cfg(not(unix))]
    {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return false;
        };
        matches!(
            ext.to_ascii_lowercase().as_str(),
            "exe"
                | "com"
                | "bat"
                | "cmd"
                | "scr"
                | "pif"
                | "msi"
                | "ps1"
                | "vbs"
                | "js"
                | "jse"
                | "wsf"
                | "wsh"
                | "cpl"
                | "msc"
                | "hta"
                | "reg"
                | "lnk"
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ExternalChange {
    Ignore,
    Conflict,
    Reload,
}

fn classify_external_change(
    saving: bool,
    dirty: bool,
    disk_mtime: Option<MTime>,
    observed: Option<MTime>,
) -> ExternalChange {
    if saving {
        return ExternalChange::Ignore;
    }
    if observed.is_some() && observed == disk_mtime {
        return ExternalChange::Ignore;
    }
    if dirty {
        ExternalChange::Conflict
    } else {
        ExternalChange::Reload
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SaveLanding {
    clean: bool,
    requeue: bool,
}

fn settle_save(ok: bool, wrote_seq: u64, current_seq: u64, pending: bool) -> SaveLanding {
    SaveLanding {
        clean: ok && wrote_seq == current_seq,
        requeue: ok && pending,
    }
}

impl NermalApp {
    /// One editor for the whole window, not one per tab — see `NermalApp::code`'s
    /// doc. `tab_code*` keep their names (every caller already reads through
    /// them) even though nothing here reads `self.tabs` any more; renaming them
    /// on top of everything else this change touches would be churn with no
    /// behaviour behind it.
    pub(crate) fn tab_code(&self) -> Option<&TabCode> {
        self.code.as_deref()
    }

    pub(crate) fn tab_code_mut(&mut self) -> Option<&mut TabCode> {
        self.code.as_deref_mut()
    }

    /// The workspace's folder is known, so the window is a project even with
    /// no terminal in it.
    pub(crate) fn has_workspace_folder(&self) -> bool {
        self.code.as_ref().is_some_and(|c| c.rooted)
    }

    /// Reconciles the window's folder with the one stored on the workspace:
    /// the stored one wins (it is what survives a restart), and one found
    /// from a pane is written back once the machine has answered.
    pub(crate) fn sync_workspace_folder(&mut self, cx: &mut Context<Self>) {
        let Some(stored) = crate::ui::tree_sync::workspace_folder(cx, self.workspace) else {
            return;
        };
        match stored {
            Some(folder) => {
                if self
                    .code
                    .as_ref()
                    .is_some_and(|c| c.synced_folder.as_ref() == Some(&folder))
                {
                    return;
                }
                let code = self.code.get_or_insert_with(|| Box::new(TabCode::new()));
                code.roots = vec![folder.clone()];
                for pinned in &code.pinned_roots {
                    if !code.roots.contains(pinned) {
                        code.roots.push(pinned.clone());
                    }
                }
                code.rooted = true;
                code.synced_folder = Some(folder);
                self.file_tree.invalidate_all();
                cx.notify();
            }
            None => {
                let Some(code) = self.code.as_mut().filter(|c| c.rooted) else {
                    return;
                };
                let Some(root) = code.roots.first().cloned() else {
                    return;
                };
                if code.synced_folder.replace(root.clone()).is_none() {
                    crate::ui::tree_sync::set_workspace_folder(cx, self.workspace, Some(root));
                }
            }
        }
    }

    pub(crate) fn tab_code_mut_or_init(&mut self) -> Option<&mut TabCode> {
        if self.tabs.get(self.active).is_none() {
            return None;
        }
        Some(self.code.get_or_insert_with(|| Box::new(TabCode::new())))
    }

    pub(crate) fn code_panel_visible(&self) -> bool {
        self.tab_code().is_some_and(|c| c.visible)
    }

    /// Every file open anywhere — there is one editor now, so this is just
    /// `self.code`'s files, but kept as its own name at the several call
    /// sites that used to have to search every tab for it.
    fn all_open_files(&self) -> impl Iterator<Item = &OpenFile> {
        self.code.iter().flat_map(|c| c.files.iter())
    }

    fn all_open_files_mut(&mut self) -> impl Iterator<Item = &mut OpenFile> {
        self.code.iter_mut().flat_map(|c| c.files.iter_mut())
    }

    fn editor_rebuild_watcher(&mut self, cx: &mut Context<Self>) {
        // Only files on the host the watch itself runs on. A path from
        // another machine — an SFTP file, say — does not exist under that
        // watcher's feet, and would either miss or, worse, match a local file
        // that happens to share its name.
        //
        // A host that cannot watch therefore gets no external-change
        // detection at all: an SFTP buffer will not notice the file changing
        // underneath it, and saving overwrites whatever is there. Catching
        // that at save time needs a "keep mine" that survives to the next
        // save, which the conflict banner does not have yet.
        let watch_host = self.spawn_host(cx);
        let files: HashSet<PathBuf> = self
            .all_open_files()
            .filter(|f| f.host.id() == watch_host)
            .map(|f| f.path.clone())
            .collect();
        let dirs: HashSet<PathBuf> = files
            .iter()
            .filter_map(|p| p.parent().map(Path::to_path_buf))
            .collect();
        self.editor.watched_files = files;
        if dirs == self.editor.watched_dirs {
            return;
        }
        self.editor.watched_dirs = dirs;
        self.editor_watch_apply(cx);
    }

    fn editor_watch_apply(&mut self, cx: &mut Context<Self>) {
        let want: Vec<PathBuf> = self.editor.watched_dirs.iter().cloned().collect();
        let Some(host) = self.active_host(cx) else {
            return;
        };

        if !self
            .editor
            .watch_host
            .as_ref()
            .is_some_and(|opened_with| Arc::ptr_eq(opened_with, &host))
        {
            self.editor.watch = None;
            self.editor.watch_host = None;
            self.editor.watch_busy = false;
            self.editor.watch_dirty = false;
        }

        if let Some(sub) = self.editor.watch.clone() {
            if self.editor.watch_busy {
                self.editor.watch_dirty = true;
                return;
            }
            self.editor.watch_busy = true;
            HostOps::run(
                host,
                cx,
                move |_| sub.set_dirs(&want),
                |app: &mut Self, result: std::io::Result<()>, cx| {
                    app.editor.watch_busy = false;
                    if let Err(e) = result {
                        log::warn!("editor: could not update the watched set: {e}");
                    }
                    if std::mem::take(&mut app.editor.watch_dirty) {
                        app.editor_watch_apply(cx);
                    }
                },
            );
            return;
        }
        if self.editor.watch_opening {
            return;
        }
        self.editor.watch_opening = true;
        let opened_host = Arc::clone(&host);
        let opened_with = self.editor.watched_dirs.clone();
        HostOps::run(
            host,
            cx,
            {
                let want = want.clone();
                move |h| h.watch(&want).map(Arc::new)
            },
            move |app, result: std::io::Result<Arc<WatchSub>>, cx| {
                app.editor.watch_opening = false;
                let sub = match result {
                    Ok(sub) => sub,
                    Err(e) => {
                        log::warn!("editor: external-change watcher unavailable: {e}");
                        return;
                    }
                };
                let events = sub.events().clone();
                app.editor.watch = Some(sub);
                app.editor.watch_host = Some(opened_host);
                cx.spawn(async move |app, cx| {
                    while let Ok(batch) = events.recv().await {
                        let ok = app.update(cx, |app, _cx| {
                            let _ = app.editor.events_tx.try_send(batch);
                        });
                        if ok.is_err() {
                            break;
                        }
                    }
                })
                .detach();
                if app.editor.watched_dirs != opened_with {
                    app.editor_watch_apply(cx);
                }
            },
        );
    }

    /// Shows what a file link in the grid pointed at.
    ///
    /// A file opens in the editor, on the line the link named; the tree
    /// follows along so "what does it say?" and "where does it live?" are
    /// answered by the same click. A directory has no contents to show, so it
    /// is only ever the tree.
    pub(crate) fn open_linked_file(
        &mut self,
        path: &Path,
        line: Option<u32>,
        column: Option<u32>,
        is_dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if is_dir {
            self.file_tree_show(path, window, cx);
            return;
        }
        self.open_file_in_editor_at(path, line, column, window, cx);
        self.file_tree_reveal_path(path, cx);
    }

    /// [`Self::open_file_in_editor`], landing the cursor on a line the caller
    /// already knows — what a `src/main.rs:120:3` in the grid was pointing at.
    pub(crate) fn open_file_in_editor_at(
        &mut self,
        path: &Path,
        line: Option<u32>,
        column: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.pending_cursor =
            line.map(|line| (path.to_path_buf(), line, column.unwrap_or(1)));
        self.open_file_in_editor(path, window, cx);
    }

    /// Moves the cursor to the position a link asked for, if the file it asked
    /// about is the one that just opened. Anything else — a different file
    /// opened in between, a file that never arrived — drops the request rather
    /// than throwing the cursor somewhere it was never meant to go.
    fn apply_pending_cursor(
        &mut self,
        host: HostId,
        requested: &Path,
        opened: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Peeked before it is taken: another file opening in the meantime must
        // not swallow a request that was never about it.
        if self
            .editor
            .pending_cursor
            .as_ref()
            .is_none_or(|(wanted, ..)| wanted != requested)
        {
            return;
        }
        let Some((_, line, column)) = self.editor.pending_cursor.take() else {
            return;
        };
        let Some(file) = self.tab_code().and_then(|c| {
            c.files
                .iter()
                .find(|f| f.host.id() == host && f.path == *opened)
        }) else {
            return;
        };
        let input = file.input.clone();
        // The grid counts from one and `Position` counts from zero, and a
        // compiler that says "line 1" means the first line either way.
        let position = Position {
            line: line.saturating_sub(1),
            character: column.saturating_sub(1),
        };
        place_cursor(input, position, CURSOR_SCROLL_ATTEMPTS, window, cx);
    }

    pub(crate) fn open_file_in_editor(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.active_host(cx) else {
            return;
        };
        self.editor_open_on_host(host, path, window, cx);
    }

    /// [`Self::open_file_in_editor`] against an explicit host — the SFTP
    /// browser's files live on a host that is never the active one.
    pub(crate) fn editor_open_on_host(
        &mut self,
        host: SharedHost,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tabs.get(self.active).is_none() {
            return;
        }
        self.raise_code_overlay();
        if self.editor_activate_open(host.id(), path, window, cx) {
            return;
        }
        let host_id = host.id();
        let p = path.to_path_buf();
        let requested = p.clone();
        HostOps::run_in(
            host.clone(),
            window,
            cx,
            move |h| -> Result<(PathBuf, String, Option<MTime>), EditorOpenError> {
                let path = h.canonicalize(&p).unwrap_or(p);
                let meta = match h.stat(&path) {
                    Ok(m) => m,
                    Err(e) => {
                        return Err(EditorOpenError::Message(t_fmt(
                            L10nKey::EditorCantOpen,
                            &[("path", &path.display().to_string()), ("e", &e.to_string())],
                        )));
                    }
                };
                if meta.len > MAX_FILE_BYTES {
                    return Err(EditorOpenError::Message(t_fmt(
                        L10nKey::EditorFileTooLarge,
                        &[
                            ("path", &path.display().to_string()),
                            ("size", &(meta.len / (1024 * 1024)).to_string()),
                        ],
                    )));
                }
                let bytes = match h.read_file(&path, MAX_FILE_BYTES) {
                    Ok(b) => b,
                    Err(e) => {
                        return Err(EditorOpenError::Message(t_fmt(
                            L10nKey::EditorCantRead,
                            &[("path", &path.display().to_string()), ("e", &e.to_string())],
                        )));
                    }
                };
                if looks_binary(&bytes) {
                    return Err(EditorOpenError::NotText(path));
                }
                let text =
                    String::from_utf8(bytes).map_err(|_| EditorOpenError::NotText(path.clone()))?;
                Ok((path, text, meta.mtime))
            },
            move |app, opened, window, cx| match opened {
                Ok((path, text, mtime)) => {
                    app.editor_install_file(host, path.clone(), text, mtime, window, cx);
                    // Against `requested`, not `path`: the host canonicalised
                    // it on the way through, and a link that named a symlink
                    // would otherwise lose the line it asked for.
                    app.apply_pending_cursor(host_id, &requested, &path, window, cx);
                }
                Err(EditorOpenError::NotText(path)) => {
                    app.open_outside_the_editor(host_id, &path, window, cx);
                }
                Err(EditorOpenError::Message(message)) => {
                    window.push_notification(message, cx);
                }
            },
        );
    }

    /// What to do with a file the built-in editor cannot show.
    ///
    /// A click on a PNG or a `.zip` meant "open this", not "tell me it is not
    /// text", and on this machine the desktop knows how. A file on another
    /// machine has nobody here to hand it to, so that one gets the words.
    ///
    /// A program does not: `open` on a Mach-O binary runs it, and a build's
    /// output is full of paths to programs. Clicking a word in a terminal
    /// must not be a way to execute one, so those keep the words too.
    fn open_outside_the_editor(
        &mut self,
        host_id: HostId,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !host_id.is_local() || !self.can_spawn_locally(cx) || is_program(path) {
            window.push_notification(
                t_fmt(
                    L10nKey::EditorBinaryFile,
                    &[("path", &path.display().to_string())],
                ),
                cx,
            );
            return;
        }
        // The OS association can fail to spawn like any other opener (#542).
        if let Err(e) = crate::terminal::view::open_file_path(path) {
            log::warn!("failed to open {}: {e}", path.display());
            window.push_notification(
                t_fmt(
                    L10nKey::LinkFileOpenFailed,
                    &[
                        ("path", &path.display().to_string()),
                        ("error", &e.to_string()),
                    ],
                ),
                cx,
            );
        }
    }

    fn editor_activate_open(
        &mut self,
        host: HostId,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(code) = self.tab_code_mut() else {
            return false;
        };
        let Some(ix) = code
            .files
            .iter()
            .position(|f| f.host.id() == host && f.path == *path)
        else {
            return false;
        };
        code.visible = true;
        code.active = ix;
        self.focus_editor(window, cx);
        self.apply_pending_cursor(host, path, path, window, cx);
        cx.notify();
        true
    }

    fn editor_install_file(
        &mut self,
        host: SharedHost,
        path: PathBuf,
        text: String,
        mtime: Option<MTime>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host_id = host.id();
        if self.editor_activate_open(host_id, &path, window, cx) {
            return;
        }
        if self.tabs.get(self.active).is_none() {
            return;
        }
        let language = language_for_path(&path);
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor(language)
                .multi_line(true)
                .tab_size(TabSize {
                    tab_size: 4,
                    hard_tabs: false,
                })
                .line_number(true)
                .searchable(true)
                .replaceable(true)
                .folding(true)
                .soft_wrap(false)
                .default_value(text)
        });
        let sub = cx.subscribe_in(&input, window, {
            let path = path.clone();
            move |this: &mut NermalApp, input, ev, window, cx| {
                if matches!(ev, InputEvent::Change) {
                    let Some(f) = this
                        .all_open_files_mut()
                        .find(|f| f.host.id() == host_id && f.path == path)
                    else {
                        return;
                    };
                    f.dirty = true;
                    f.edit_seq = f.edit_seq.wrapping_add(1);
                    if cx.global::<crate::core::config::Config>().editor_auto_save {
                        this.editor_schedule_auto_save(input.entity_id(), window, cx);
                    }
                    cx.notify();
                }
            }
        });
        let code = self.code.get_or_insert_with(|| Box::new(TabCode::new()));
        let observe = cx.observe(&input, |_, _, cx| cx.notify());
        code.files.insert(
            0,
            OpenFile {
                path,
                host,
                input,
                dirty: false,
                disk_mtime: mtime,
                edit_seq: 0,
                saving: None,
                save_pending: false,
                save_then_close: false,
                reload_seq: 0,
                conflict: false,
                preview: false,
                wrap: false,
                preview_scroll: gpui::ScrollHandle::new(),
                _sub: sub,
                _observe: observe,
            },
        );
        code.active = 0;
        code.visible = true;
        self.editor_rebuild_watcher(cx);
        self.focus_editor(window, cx);
        cx.notify();
    }

    pub(crate) fn toggle_code_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Read before anything below mutates `tab.code` — `document_front`
        // is the *effective* on/off state (a never-touched tab reads as on,
        // showing its empty state, unless it was already dismissed), which is
        // what the toggle has to flip. The raw `code.visible` this used to
        // read stayed `false` for an untouched tab and made the first press
        // "open" a dock the reader was already looking at.
        let showing_code = self.document_front() == Some(crate::ui::app::OverlayTop::Code);
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let buried = tab.overlay_top == crate::ui::app::OverlayTop::Diff
            && tab.diff_overlay.is_some()
            && showing_code;
        tab.overlay_top = crate::ui::app::OverlayTop::Code;
        if buried {
            self.focus_editor(window, cx);
            cx.notify();
            return;
        }
        if self.tabs.get(self.active).is_none() {
            return;
        }
        if showing_code {
            // `document_dismissed` rather than only `code.visible = false`:
            // a tab with no file open was never explicitly "opened" in the
            // first place, so there is nothing for the raw flag to record —
            // the default has to be turned off somewhere that survives an
            // unrelated `TabCode` getting created later (pinning a root,
            // tracking one) without silently popping the empty state back.
            if let Some(code) = self.code.as_mut() {
                code.visible = false;
            }
            self.document_dismissed = true;
            self.file_tree.editing = None;
            self.focus_active(window, cx);
            cx.notify();
            return;
        }
        self.document_dismissed = false;
        let code = self.code.get_or_insert_with(|| Box::new(TabCode::new()));
        code.visible = true;
        self.file_tree_refresh_roots(window, cx);
        if self.tab_code().is_some_and(|c| c.active_file().is_some()) {
            self.focus_editor(window, cx);
        } else {
            // With no file to show, the panel says "Open a file from the file
            // tree" and hands the tree the focus — but nothing was putting the
            // tree on screen, so ⌘⇧E on a fresh tab opened an empty editor
            // pointing at a panel the reader could not see or reach from
            // there. Reveal it, then focus it.
            self.reveal_file_tree_sidebar(cx);
            self.file_tree.focus_handle.focus(window, cx);
        }
        cx.notify();
    }

    fn raise_code_overlay(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.overlay_top = crate::ui::app::OverlayTop::Code;
        }
    }

    fn focus_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(f) = self.tab_code().and_then(|c| c.active_file()) {
            f.input.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    pub(crate) fn editor_has_focus(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.code_panel_visible()
            && self
                .tab_code()
                .and_then(|c| c.active_file())
                .is_some_and(|f| {
                    f.input
                        .read(cx)
                        .focus_handle(cx)
                        .contains_focused(window, cx)
                })
    }

    pub(crate) fn editor_save_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self
            .tab_code()
            .and_then(|c| c.active_file())
            .map(|f| f.input.entity_id())
        else {
            return;
        };
        self.editor_save_file(id, false, window, cx);
    }

    /// Writes `id`'s buffer `AUTO_SAVE_DEBOUNCE` after the edit that asked
    /// for this, unless a later edit or a save already overtook it.
    ///
    /// One timer per keystroke rather than a per-file "is one already
    /// pending" flag: `edit_seq` already tells a stale save apart from a
    /// current one (`editor_save_file` uses it the same way for the disk
    /// write itself), so the extra flag would just be a second copy of that
    /// check.
    fn editor_schedule_auto_save(
        &mut self,
        id: gpui::EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(f) = self.editor_file_mut(id) else {
            return;
        };
        let seq = f.edit_seq;
        cx.spawn_in(window, async move |app, cx| {
            cx.background_executor().timer(AUTO_SAVE_DEBOUNCE).await;
            let _ = app.update_in(cx, |app, window, cx| {
                let due = app
                    .editor_file_mut(id)
                    .is_some_and(|f| f.edit_seq == seq && f.dirty && !f.conflict);
                if due {
                    app.editor_save_file(id, false, window, cx);
                }
            });
        })
        .detach();
    }

    fn editor_save_file(
        &mut self,
        id: gpui::EntityId,
        then_close: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The file's own host, not the active one: the buffer keeps pointing
        // at the machine it was read from, however the focus has moved since.
        let Some(host) = self.editor_file_mut(id).map(|f| f.host.clone()) else {
            return;
        };
        let Some(f) = self.editor_file_mut(id) else {
            return;
        };
        f.save_then_close |= then_close;
        if f.saving.is_some() {
            f.save_pending = true;
            return;
        }
        let seq = f.edit_seq;
        f.saving = Some(seq);
        let text = f.input.read(cx).text().to_string();
        let target = f.path.clone();
        let host_id = host.id();
        let saved_in = target.parent().map(std::path::Path::to_path_buf);
        HostOps::run_in(
            host,
            window,
            cx,
            move |h| h.write_file(&target, text.as_bytes()).map(|m| m.mtime),
            move |app, result: std::io::Result<Option<MTime>>, window, cx| {
                let Some(f) = app.editor_file_mut(id) else {
                    return;
                };
                f.saving = None;
                let landing = settle_save(
                    result.is_ok(),
                    seq,
                    f.edit_seq,
                    std::mem::take(&mut f.save_pending),
                );
                let wrote = result.is_ok();
                match result {
                    Ok(mtime) => {
                        f.disk_mtime = mtime;
                    }
                    Err(e) => {
                        // "Save failed" did not say which file, and with more
                        // than one editor tab open that is the first thing you
                        // need to know.
                        let name = f
                            .path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| f.path.display().to_string());
                        let context = t_fmt(L10nKey::EditorSaveFailed, &[("name", &name)]);
                        HostOps::notify_err(window, cx, &context, &e);
                    }
                }
                if landing.clean {
                    f.dirty = false;
                    f.conflict = false;
                }
                // A save is a working-tree edit the `.git` watch cannot see,
                // and the file tree only sees it while it happens to be showing
                // that directory.
                if wrote && let Some(dir) = &saved_in {
                    app.scm_invalidate_cwd(host_id, dir, cx);
                }
                if landing.requeue {
                    app.editor_save_file(id, false, window, cx);
                    cx.notify();
                    return;
                }
                let close = app
                    .editor_file_mut(id)
                    .is_some_and(|f| std::mem::take(&mut f.save_then_close) && !f.dirty);
                if close && let Some(ix) = app.editor_file_position(id) {
                    app.editor_remove_file_in(ix, cx);
                }
                cx.notify();
            },
        );
        cx.notify();
    }

    fn editor_file_mut(&mut self, id: gpui::EntityId) -> Option<&mut OpenFile> {
        self.all_open_files_mut()
            .find(|f| f.input.entity_id() == id)
    }

    fn editor_file_position(&self, id: gpui::EntityId) -> Option<usize> {
        self.code
            .as_deref()?
            .files
            .iter()
            .position(|f| f.input.entity_id() == id)
    }

    pub(crate) fn editor_close_file(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(f) = self.tab_code().and_then(|c| c.files.get(ix)) else {
            return;
        };
        if !f.dirty {
            self.editor_remove_file(ix, cx);
            return;
        }
        let name = f.label();
        let answer = window.prompt(
            PromptLevel::Warning,
            &t_fmt(L10nKey::EditorUnsavedChanges, &[("name", &name)]),
            None,
            // Cancel sits between Save and Discard on purpose. The platform
            // renders the first button as the default and lays the rest out
            // beside it, so Discard was landing directly next to the key that
            // Return presses. Apple separates them for exactly this reason.
            // Three answers, so the shared helper does not fit: Save keeps
            // index 0 (rightmost, Return), Cancel takes Escape, and Discard
            // sits on the far left where nothing lands by reflex.
            &[
                gpui::PromptButton::ok(t(L10nKey::Save)),
                gpui::PromptButton::cancel(t(L10nKey::Cancel)),
                gpui::PromptButton::ok(t(L10nKey::EditorDiscard)),
            ],
            cx,
        );
        let id = f.input.entity_id();
        cx.spawn_in(window, async move |app, cx| {
            let Ok(choice) = answer.await else { return };
            let _ = app.update_in(cx, |app, window, cx| match choice {
                0 => app.editor_save_file(id, true, window, cx),
                2 => {
                    if let Some(ix) = app.editor_file_position(id) {
                        app.editor_remove_file_in(ix, cx);
                    }
                }
                _ => {}
            });
        })
        .detach();
    }

    /// Closes every open file except `keep_ix`, by identity rather than
    /// index — each close can shift the indices of the ones still open, and
    /// a dirty file's close does not even finish until its save/discard
    /// prompt answers, well after this loop returns.
    fn editor_close_others(&mut self, keep_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(code) = self.tab_code() else {
            return;
        };
        let Some(keep_id) = code.files.get(keep_ix).map(|f| f.input.entity_id()) else {
            return;
        };
        let ids: Vec<_> = code
            .files
            .iter()
            .map(|f| f.input.entity_id())
            .filter(|id| *id != keep_id)
            .collect();
        for id in ids {
            if let Some(ix) = self.editor_file_position(id) {
                self.editor_close_file(ix, window, cx);
            }
        }
    }

    /// Closes every open file, by identity — see [`Self::editor_close_others`].
    fn editor_close_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(code) = self.tab_code() else {
            return;
        };
        let ids: Vec<_> = code.files.iter().map(|f| f.input.entity_id()).collect();
        for id in ids {
            if let Some(ix) = self.editor_file_position(id) {
                self.editor_close_file(ix, window, cx);
            }
        }
    }

    pub(crate) fn editor_close_active_if_focused(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.editor_has_focus(window, cx) {
            return false;
        }
        let Some(code) = self.tab_code_mut() else {
            return false;
        };
        if code.files.is_empty() {
            code.visible = false;
            cx.notify();
            return true;
        }
        let active = code.active;
        self.editor_close_file(active, window, cx);
        true
    }

    fn editor_remove_file(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.editor_remove_file_in(ix, cx);
    }

    fn editor_remove_file_in(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(code) = self.code.as_deref_mut() else {
            return;
        };
        if ix >= code.files.len() {
            return;
        }
        code.files.remove(ix);
        if code.active >= ix && code.active > 0 {
            code.active -= 1;
        }
        self.editor_rebuild_watcher(cx);
        cx.notify();
    }

    pub(crate) fn editor_handle_external_change(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.active_host(cx) else {
            return;
        };
        let host_id = host.id();
        let p = path.to_path_buf();
        let landed = p.clone();
        HostOps::run_in(
            host,
            window,
            cx,
            move |h| h.stat(&p).ok().and_then(|m| m.mtime),
            move |app, mtime, window, cx| {
                app.editor_apply_external_change(host_id, &landed, mtime, window, cx)
            },
        );
    }

    fn editor_apply_external_change(
        &mut self,
        host: HostId,
        path: &Path,
        mtime: Option<MTime>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut reload: Vec<usize> = Vec::new();
        let mut changed = false;
        if let Some(code) = self.code.as_deref_mut() {
            for (ix, f) in code.files.iter_mut().enumerate() {
                if f.host.id() != host || f.path != *path {
                    continue;
                }
                match classify_external_change(f.saving.is_some(), f.dirty, f.disk_mtime, mtime) {
                    ExternalChange::Ignore => {}
                    ExternalChange::Conflict => {
                        f.conflict = true;
                        changed = true;
                    }
                    ExternalChange::Reload => reload.push(ix),
                }
            }
        }
        for ix in reload {
            self.editor_reload_from_disk(ix, window, cx);
        }
        if changed {
            cx.notify();
        }
    }

    pub(crate) fn editor_reload_from_disk(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(f) = self.code.as_deref_mut().and_then(|c| c.files.get_mut(ix)) else {
            return;
        };
        let target = f.path.clone();
        let host = f.host.clone();
        let id = f.input.entity_id();
        f.reload_seq = f.reload_seq.wrapping_add(1);
        let seq = f.reload_seq;
        HostOps::run_in(
            host,
            window,
            cx,
            move |h| {
                let bytes = h.read_file(&target, MAX_FILE_BYTES)?;
                let text = String::from_utf8(bytes).map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "not valid UTF-8")
                })?;
                let mtime = h.stat(&target).ok().and_then(|m| m.mtime);
                Ok((text, mtime))
            },
            move |app, result: std::io::Result<(String, Option<MTime>)>, window, cx| {
                let Some(f) = app.editor_file_mut(id) else {
                    return;
                };
                if f.reload_seq != seq {
                    return;
                }
                let Ok((text, mtime)) = result else {
                    f.dirty = true;
                    f.conflict = false;
                    cx.notify();
                    return;
                };
                f.disk_mtime = mtime;
                f.dirty = false;
                f.conflict = false;
                f.edit_seq = f.edit_seq.wrapping_add(1);
                let input = f.input.clone();
                input.update(cx, |input, cx| input.set_value(text, window, cx));
                cx.notify();
            },
        );
    }
}

impl NermalApp {
    pub(crate) fn render_code_overlay(
        &mut self,
        chrome: DocumentChrome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.code_panel_visible() {
            return None;
        }
        let body = match self.tab_code().and_then(|c| c.active_file()) {
            None => self.render_editor_empty(cx).into_any_element(),
            Some(f) if f.preview => {
                let markdown = f.input.read(cx).text().to_string();
                let scroll = f.preview_scroll.clone();
                // The bar's wrapper takes its height from `flex_1`, so it needs
                // a column with a definite height to grow inside — hand it one
                // rather than dropping it straight into the overlay, or the
                // pane sizes to its content and there is nothing left to
                // scroll.
                v_flex()
                    .size_full()
                    .child(crate::ui::scrollbar::with_vertical_scrollbar(
                        "editor-md-preview-scrollbar",
                        div()
                            .id("editor-md-preview")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&scroll)
                            .px_4()
                            .py_3()
                            .child(gpui_component::text::TextView::markdown(
                                "editor-md-preview-body",
                                markdown,
                            )),
                        &scroll,
                    ))
                    .into_any_element()
            }
            Some(f) => {
                let input = f.input.clone();
                Input::new(&input)
                    .appearance(false)
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(cx.theme().mono_font_size)
                    .size_full()
                    .into_any_element()
            }
        };
        let conflict_banner = self
            .tab_code()
            .and_then(|c| c.active_file())
            .filter(|f| f.conflict)
            .map(|_| self.render_editor_conflict_banner(cx));

        let header = chrome
            .renders_own_header()
            .then(|| self.render_editor_header(chrome, window, cx));
        let editor_col = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .children(header)
            .when_some(conflict_banner, |this, b| this.child(b))
            .child(div().flex_1().min_h_0().child(body));

        // The panel's own paint is the same either way; only the box is not.
        // Filling the workspace means stopping the window's translucency and
        // repainting the theme image the root's copy now sits under; docking
        // means sitting in the same plane as the right panel, which the column
        // wrapper has already painted.
        let shell = v_flex().id("code-panel");
        let shell = match chrome {
            DocumentChrome::Fill => shell
                .absolute()
                .inset_0()
                .occlude()
                // Opaque on purpose: this overlay covers the whole workspace
                // (everything but the detail panel) and an open file must never
                // let the window translucency / backdrop material show through
                // it. The preset's gradient fill is preserved, just with
                // alpha 1 — the same paint the settings overlay uses. The
                // theme background image is repainted on top of it, since the
                // root's copy now sits below this fill.
                .bg(crate::ui::theme::overlay_background(cx))
                .children(crate::ui::app::overlay_surface_layers(cx)),
            DocumentChrome::Dock | DocumentChrome::DockHoisted => shell.size_full().min_w_0(),
        };
        Some(
            shell
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                    if ev.keystroke.key == "escape" {
                        this.toggle_code_panel(window, cx);
                    }
                }))
                .child(h_flex().flex_1().min_h_0().w_full().child(editor_col))
                .child(self.render_code_status_bar(window, cx))
                .into_any_element(),
        )
    }

    /// The editor header alone, for the strip above a docked column.
    pub(crate) fn render_editor_header_only(
        &self,
        chrome: DocumentChrome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        self.render_editor_header(chrome, window, cx)
            .into_any_element()
    }

    /// One chip per open file, so switching between them (README.md,
    /// LICENSE, …) is a click rather than a trip through the tree every
    /// time. Reuses the terminal tab strip's chip-close visuals
    /// (`tab_strip::hit_target`) rather than drawing new ones.
    fn render_editor_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let files: Vec<(SharedString, bool)> = self
            .tab_code()
            .map(|c| c.files.iter().map(|f| (f.label(), f.dirty)).collect())
            .unwrap_or_default();
        let active_ix = self.tab_code().map(|c| c.active).unwrap_or(0);
        let mut row = h_flex()
            .id("editor-tabs")
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .gap_1()
            .overflow_x_scroll();
        if files.is_empty() {
            return row.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(t(L10nKey::EditorNoFileOpen)),
            );
        }
        for (i, (label, dirty)) in files.into_iter().enumerate() {
            let is_active = i == active_ix;
            let menu_app = cx.entity().downgrade();
            let mut chip = h_flex()
                .id(("editor-tab", i))
                .flex_shrink_0()
                .items_center()
                .gap_1()
                .h(px(crate::ui::app::TILE_SIZE))
                .px_2()
                .rounded_md()
                .cursor_pointer()
                .when(is_active, |d| d.bg(cx.theme().secondary))
                .when(!is_active, |d| {
                    d.hover(|s| s.bg(cx.theme().muted.opacity(0.6)))
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    if let Some(code) = this.tab_code_mut() {
                        code.active = i;
                    }
                    this.focus_editor(window, cx);
                    cx.notify();
                }))
                .context_menu(move |menu, _window, cx| {
                    Self::editor_tab_context_menu(menu, i, &menu_app, cx)
                })
                .child(
                    div()
                        .max_w(px(160.))
                        .min_w_0()
                        .text_ellipsis()
                        .text_sm()
                        .child(label),
                );
            if dirty {
                chip = chip.child(
                    div()
                        .flex_none()
                        .size(px(6.))
                        .rounded_full()
                        .bg(cx.theme().warning),
                );
            }
            chip = chip.child(
                crate::ui::tab_strip::hit_target(
                    Button::new(("editor-tab-close", i))
                        .icon(IconName::Close)
                        .ghost()
                        .xsmall(),
                )
                .tooltip(t(L10nKey::TabContextCloseTab))
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.editor_close_file(i, window, cx);
                })),
            );
            row = row.child(chip);
        }
        row
    }

    fn editor_tab_context_menu(
        menu: PopupMenu,
        ix: usize,
        app: &gpui::WeakEntity<Self>,
        cx: &gpui::App,
    ) -> PopupMenu {
        let file_count = app
            .upgrade()
            .and_then(|e| e.read(cx).tab_code().map(|c| c.files.len()))
            .unwrap_or(0);
        menu.min_w(px(200.))
            .item(
                PopupMenuItem::new(t(L10nKey::TabContextCloseTab)).on_click({
                    let app = app.clone();
                    move |_, window, cx| {
                        let _ = app.update(cx, |this, cx| this.editor_close_file(ix, window, cx));
                    }
                }),
            )
            .item(
                PopupMenuItem::new(t(L10nKey::AppMenuCloseOtherTabs))
                    .disabled(file_count <= 1)
                    .on_click({
                        let app = app.clone();
                        move |_, window, cx| {
                            let _ =
                                app.update(cx, |this, cx| this.editor_close_others(ix, window, cx));
                        }
                    }),
            )
            .item(
                PopupMenuItem::new(t(L10nKey::EditorCloseAllTabs)).on_click({
                    let app = app.clone();
                    move |_, window, cx| {
                        let _ = app.update(cx, |this, cx| this.editor_close_all(window, cx));
                    }
                }),
            )
    }

    fn render_editor_header(
        &self,
        chrome: DocumentChrome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        // `TITLE_BAR_LEAD` is the room macOS's traffic lights need. Only a
        // header that starts at the left edge of the window has them to clear,
        // and a docked column never does.
        let lead = if self.left_panel_open(cx) || chrome.is_dock() {
            crate::ui::app::CONTENT_INSET
        } else {
            crate::ui::app::TITLE_BAR_LEAD
        };
        let row = h_flex().id("editor-header");
        let row = if chrome.header_is_title_strip() {
            crate::ui::app::title_bar_drag(row, "editor-header", window, cx)
        } else {
            row
        };
        let menu_app = cx.entity().downgrade();
        let tabs = self.render_editor_tabs(cx);
        row.flex_none()
            .h(px(crate::ui::app::TITLE_BAR_HEIGHT))
            .items_center()
            .gap_1p5()
            .pl(px(lead))
            .pr(px(crate::ui::app::tile_trailing_inset()))
            .border_b_1()
            .border_color(cx.theme().border)
            .child(tabs)
            .child(
                div().occlude().flex_shrink_0().child(
                    crate::ui::tab_strip::chrome_tile_sized(
                        Button::new("editor-panel-close").icon(Icon::new(IconName::Close)),
                        crate::ui::app::TILE_SIZE,
                        crate::ui::app::TILE_GLYPH_LINE,
                        false,
                        cx,
                    )
                    .rounded_lg()
                    .tooltip(t(L10nKey::EditorBackToTerminal))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_code_panel(window, cx);
                    })),
                ),
            )
            .context_menu(move |menu, _window, cx| {
                NermalApp::document_header_menu(menu, &menu_app, cx)
            })
    }

    fn render_code_status_bar(&self, _window: &Window, cx: &mut Context<Self>) -> gpui::Div {
        // The roots below belong to this window's own machine. A file read
        // over SFTP is on another one, where they mean nothing, so it shows
        // its own full path rather than borrowing the local repo's name.
        let tree_host = self.spawn_host(cx);
        let code = self.tab_code();
        let muted = cx.theme().muted_foreground;
        let path_text: Option<SharedString> = code.map(|c| {
            let repo = c
                .roots
                .first()
                .and_then(|r| r.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            match c.active_file() {
                Some(f) if f.host.id() != tree_host => f.path.display().to_string().into(),
                Some(f) => {
                    let rel = c
                        .roots
                        .iter()
                        .find_map(|r| f.path.strip_prefix(r).ok())
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| f.label().to_string());
                    format!("{repo} › {rel}").into()
                }
                None => repo.into(),
            }
        });
        let active = code.and_then(|c| c.active_file());
        let cursor: Option<SharedString> = active.map(|f| {
            let pos = f.input.read(cx).cursor_position();
            t_fmt(
                L10nKey::EditorLnCol,
                &[
                    ("line", &(pos.line + 1).to_string()),
                    ("column", &(pos.character + 1).to_string()),
                ],
            )
            .into()
        });
        let wrap: Option<bool> = active.map(|f| f.wrap);
        let is_markdown = active.is_some_and(|f| language_for_path(&f.path) == "markdown");
        let preview = active.is_some_and(|f| f.preview);
        let auto_save = active.map(|_| cx.global::<crate::core::config::Config>().editor_auto_save);

        h_flex()
            .flex_none()
            .w_full()
            .h(px(26.))
            .items_center()
            .gap_3()
            .px_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .text_xs()
            .text_color(muted)
            .when_some(path_text, |this, t| {
                this.child(div().min_w_0().text_ellipsis().child(t))
            })
            .child(div().flex_1())
            .when(is_markdown, |this| {
                this.child(
                    Button::new("status-md-preview")
                        .label(if preview {
                            t(L10nKey::EditorEdit)
                        } else {
                            t(L10nKey::EditorPreview)
                        })
                        .custom(crate::ui::tab_strip::chrome_tile_variant(cx))
                        .xsmall()
                        .on_click(cx.listener(|this, _, _w, cx| {
                            if let Some(code) = this.tab_code_mut() {
                                let ix = code.active;
                                if let Some(f) = code.files.get_mut(ix) {
                                    f.preview = !f.preview;
                                    cx.notify();
                                }
                            }
                        })),
                )
            })
            .when_some(wrap, |this, wrap| {
                this.child(
                    Button::new("status-wrap")
                        .label(if wrap {
                            t(L10nKey::EditorWrapOn)
                        } else {
                            t(L10nKey::EditorWrapOff)
                        })
                        .custom(crate::ui::tab_strip::chrome_tile_variant(cx))
                        .xsmall()
                        .on_click(cx.listener(|this, _, window, cx| {
                            let Some(code) = this.tab_code_mut() else {
                                return;
                            };
                            let ix = code.active;
                            if let Some(f) = code.files.get_mut(ix) {
                                f.wrap = !f.wrap;
                                let wrap = f.wrap;
                                f.input.clone().update(cx, |st, cx| {
                                    st.set_soft_wrap(wrap, window, cx);
                                });
                            }
                        })),
                )
            })
            .when_some(auto_save, |this, on| {
                this.child(
                    Button::new("status-auto-save")
                        .label(if on {
                            t(L10nKey::EditorAutoSaveOn)
                        } else {
                            t(L10nKey::EditorAutoSaveOff)
                        })
                        .custom(crate::ui::tab_strip::chrome_tile_variant(cx))
                        .xsmall()
                        .on_click(cx.listener(|this, _, window, cx| {
                            let on = !cx.global::<crate::core::config::Config>().editor_auto_save;
                            this.update_config(cx, |cfg| cfg.editor_auto_save = on);
                            if on {
                                let ids: Vec<gpui::EntityId> = this
                                    .all_open_files()
                                    .filter(|f| f.dirty && !f.conflict)
                                    .map(|f| f.input.entity_id())
                                    .collect();
                                for id in ids {
                                    this.editor_schedule_auto_save(id, window, cx);
                                }
                            }
                        })),
                )
            })
            .when_some(cursor, |this, t| this.child(div().child(t)))
    }

    pub(crate) fn render_editor_empty(&self, cx: &Context<Self>) -> gpui::Div {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                Icon::new(IconName::File)
                    .large()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::ui::i18n::t(
                        crate::ui::i18n::L10nKey::EditorEmptyExploreHint,
                    )),
            )
    }

    fn render_editor_conflict_banner(&self, cx: &mut Context<Self>) -> AnyElement {
        let ix = self.tab_code().map(|c| c.active).unwrap_or(0);
        h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .bg(cx.theme().warning.opacity(0.15))
            .border_b_1()
            .border_color(cx.theme().border)
            .text_sm()
            .child(div().flex_1().child(crate::ui::i18n::t(
                crate::ui::i18n::L10nKey::FileChangedOnDisk,
            )))
            .child(
                Button::new("editor-conflict-reload")
                    .label(crate::ui::i18n::t(crate::ui::i18n::L10nKey::Reload))
                    .small()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.editor_reload_from_disk(ix, window, cx);
                    })),
            )
            .child(
                Button::new("editor-conflict-keep")
                    .label(crate::ui::i18n::t(crate::ui::i18n::L10nKey::KeepMine))
                    .ghost()
                    .small()
                    .on_click(cx.listener(move |this, _, _w, cx| {
                        if let Some(f) = this.tab_code_mut().and_then(|c| c.files.get_mut(ix)) {
                            f.conflict = false;
                            cx.notify();
                        }
                    })),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Handing a file the editor cannot read to the desktop is how a click
    /// opens a PNG. It must not be how a click runs a build's output.
    #[cfg(unix)]
    #[test]
    fn a_file_the_desktop_would_run_is_not_handed_to_it() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("nermal-program-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let image = dir.join("shot.png");
        let program = dir.join("built");
        std::fs::write(&image, b"\x89PNG\0\0").expect("write image");
        std::fs::write(&program, b"\x7fELF\0\0").expect("write program");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("mark executable");

        assert!(!is_program(&image), "a picture is only ever shown");
        assert!(is_program(&program), "a binary would be launched");
        assert!(
            !is_program(&dir),
            "a directory is not a program, whatever its mode says"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn language_map_covers_common_extensions() {
        for (path, lang) in [
            ("a/b/main.rs", "rust"),
            ("x.tsx", "tsx"),
            ("x.jsx", "javascript"),
            ("x.yml", "yaml"),
            ("Makefile", "make"),
            ("CMakeLists.txt", "cmake"),
            (".zshrc", "bash"),
            ("notes.md", "markdown"),
            ("query.SQL", "sql"),
            ("unknown.xyz", "text"),
            ("no_ext", "text"),
        ] {
            assert_eq!(language_for_path(Path::new(path)), lang, "path {path}");
        }
    }

    #[test]
    fn binary_sniff_flags_nul_bytes_only() {
        assert!(looks_binary(b"\x7fELF\x00\x01"));
        assert!(!looks_binary("plain text\nwith lines".as_bytes()));
        assert!(!looks_binary("中文 UTF-8 内容".as_bytes()));
    }

    fn t(secs: i64, nanos: u32) -> Option<MTime> {
        Some(MTime { secs, nanos })
    }

    #[test]
    fn external_changes_are_told_apart_from_our_own_saves() {
        let ours = t(100, 0);

        assert_eq!(
            classify_external_change(false, false, ours, ours),
            ExternalChange::Ignore
        );

        assert_eq!(
            classify_external_change(false, false, ours, t(101, 0)),
            ExternalChange::Reload
        );

        assert_eq!(
            classify_external_change(false, true, ours, t(101, 0)),
            ExternalChange::Conflict
        );

        assert_eq!(
            classify_external_change(false, false, t(100, 0), t(100, 1)),
            ExternalChange::Reload
        );

        assert_eq!(
            classify_external_change(true, false, ours, t(101, 0)),
            ExternalChange::Ignore
        );

        assert_eq!(
            classify_external_change(false, false, None, None),
            ExternalChange::Reload
        );
    }

    #[test]
    fn a_landed_save_only_cleans_a_buffer_that_did_not_move() {
        assert_eq!(
            settle_save(true, 7, 7, false),
            SaveLanding {
                clean: true,
                requeue: false
            }
        );

        assert_eq!(
            settle_save(true, 7, 9, false),
            SaveLanding {
                clean: false,
                requeue: false
            }
        );

        assert_eq!(
            settle_save(true, 7, 9, true),
            SaveLanding {
                clean: false,
                requeue: true
            }
        );

        assert_eq!(
            settle_save(false, 7, 7, true),
            SaveLanding {
                clean: false,
                requeue: false
            }
        );
    }
}
