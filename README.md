# nermal

A terminal workbench with a built-in editor: persistent sessions, remote work, and AI agent support.

Pure Rust · GPU rendering on Zed's gpui · VT core from Alacritty

## Why

A background server owns your shells and panes, not the window.

- **Persistent sessions** — quit or reboot; shells and agent sessions keep running
- **Agent-aware** — status, notifications, and git context for Claude Code, Codex, and other CLI agents
- **Built-in editor** — open a file and it docks above the terminal, with syntax highlighting, auto-save, and conflict detection
- **Remote development** — files, repos, and panes stay on the remote machine over a native SSH stack
- **Git integration** — diffs, staging, and worktrees without leaving the window

## Install

Native builds for macOS, Windows, and Linux on [**Releases**](https://github.com/l0ng-ai/nermal/releases):

| | | |
|---|---|---|
| **macOS** | `…-macos-arm64.dmg` · `…-x86_64.dmg` | drag into Applications |
| **Windows** | `…-setup.exe` · portable `….zip` | |
| **Linux** | `…-x86_64.AppImage` | `chmod +x` and run |

## Documentation

Full documentation lives in [**`docs/`**](docs/), including [keyboard shortcuts](docs/reference/keyboard-shortcuts.mdx), [config.json](docs/reference/configuration.mdx), and the [CLI reference](docs/cli/reference.mdx).

## License

[Apache-2.0](LICENSE)

---

Forked from [l0ng-ai/tty7](https://github.com/l0ng-ai/tty7).
