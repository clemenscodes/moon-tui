# moon-tui

A terminal UI for browsing and running [Moon](https://moonrepo.dev) workspace tasks.

## Features

- Browse projects and tasks in your Moon workspace
- Run tasks with live output streaming
- Interactive task execution with PTY support
- VT100 terminal emulation for rich output

## Usage

```bash
moon-tui
```

## Keybindings

| Key | Action |
|-----|--------|
| `h`/`l` or `←`/`→` | Navigate between Projects and Tasks panels |
| `j`/`k` or `↓`/`↑` | Move selection up/down |
| `Enter` | Run selected task |
| `I` | Run task interactively (PTY) |
| `R` | Force-run task (bypass cache) |
| `r` | Refresh project/task list |
| `?` | Toggle help overlay |
| `q` or `Ctrl-C` | Quit |

## Installation

### Nix (recommended)

Add as a flake input:

```nix
inputs.moon-tui.url = "github:clemenscodes/moon-tui";
```

Then use the package:

```nix
inputs.moon-tui.packages.${system}.moon-tui
```

Or run directly:

```bash
nix run github:clemenscodes/moon-tui
```

### Cargo

```bash
cargo install --git https://github.com/clemenscodes/moon-tui
```

## Requirements

- [Moon](https://moonrepo.dev) must be installed and in PATH
- Must be run from within a Moon workspace
