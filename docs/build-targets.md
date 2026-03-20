---
hide:
  - toc
---

## Build Targets

wakterm ships pre-built binaries for three platforms.
Upstream WezTerm supports a wider matrix, though its last tagged release
(`20240203-110809`) is from **February 2024**, and all packages across every
channel still ship that version.

| Platform / Format | WezTerm | wakterm | Notes |
|---|:-:|:-:|---|
| **macOS** (.zip, universal) | :material-check: | :material-check: | |
| **macOS** Homebrew cask | :material-check: | :material-check: | wakterm uses its own tap |
| **macOS** MacPorts | :material-check: | :material-close: | |
| **Windows** (setup.exe + zip) | :material-check: | :material-check: | |
| **Windows** winget | :material-check: | :material-check: | `wakamex.wakterm` |
| **Windows** Scoop | :material-check: | :material-close: | |
| **Windows** Chocolatey | :material-check: | :material-close: | |
| **Ubuntu/Debian** (.deb) | :material-check: | :material-check: | |
| **Fedora** (.rpm) | :material-check: | :material-check: | CI builds on fedora-latest |
| **openSUSE** | :material-check: | :material-close: | |
| **Arch Linux** (AUR) | :material-check: | :material-close: | 19 votes upstream |
| **Alpine** (apk) | :material-check: | :material-close: | |
| **Flatpak** (Flathub) | :material-check: | :material-close: | ~3.2k installs/month, 124k total upstream |
| **AppImage** | :material-check: | :material-close: | |
| **Linuxbrew** | :material-check: | :material-close: | |
| **Nix / NixOS** | :material-check: | :material-close: | flake in tree, not built in CI |
| **FreeBSD** | :material-check: | :material-close: | |
| **NetBSD** | :material-check: | :material-close: | |

All upstream WezTerm packages are frozen at the February 2024 release.

### Relationship to upstream

96% of wakterm's Rust source is upstream WezTerm code.
The fork adds ~15k net lines focused on mux reliability, session persistence, and agent harnesses.

### Why fewer targets?

wakterm focuses build and test effort on the three platforms its maintainer
uses daily (macOS, Ubuntu, Windows).  Packaging scripts for other targets
are still in the tree from upstream and can be revived if there is demand.

If you need a platform that wakterm doesn't ship yet, the upstream
[WezTerm packages](https://wezterm.org/installation) are available,
but note they haven't been updated since February 2024.
