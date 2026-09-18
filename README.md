# Nermal

A terminal workbench with a built-in editor: persistent sessions, remote work, and AI agent support.

Pure Rust · GPU rendering on Zed's gpui · VT core from Alacritty

## Why

Well i made this for my own usecase but if you have any feature request, feel free to drop an issue.

A background server owns your shells and panes, not the window.

- **Persistent sessions** — quit or reboot; shells and agent sessions keep running
- **Agent-aware** — status, notifications, and git context for Claude Code, Codex, and other CLI agents
- **Built-in editor** — open a file and it docks above the terminal, with syntax highlighting, auto-save, and conflict detection
- **Remote development** — files, repos, and panes stay on the remote machine over a native SSH stack
- **Git integration** — diffs, staging, and worktrees without leaving the window

## Install

Native builds for macOS, Windows, and Linux on [**Releases**](https://github.com/nermalcat69/nermal-ide/releases):

| | | |
|---|---|---|
| **macOS** | `…-macos-arm64.dmg` · `…-x86_64.dmg` | drag into Applications |
| **Windows** | `…-setup.exe` · portable `….zip` | |
| **Linux** | `…-x86_64.AppImage` | `chmod +x` and run |

macOS builds aren't notarized yet, so Gatekeeper will refuse to open `Nermal.app` the first time. Clear the quarantine flag once after installing:

Try opening the app and it may prompt you that this application has some issue.

You can go to settings and then privacy and allow open anyway for this application.

or you can do this in your terminal if the solution above doesn't work

```sh
xattr -dr com.apple.quarantine /Applications/Nermal.app
```


## Documentation

Full documentation lives in [**`docs/`**](docs/), including [keyboard shortcuts](docs/reference/keyboard-shortcuts.mdx), [config.json](docs/reference/configuration.mdx), and the [CLI reference](docs/cli/reference.mdx).

## License

[Apache-2.0](LICENSE)

---

Intial Codebase was forked from [l0ng-ai/tty7](https://github.com/l0ng-ai/tty7).
