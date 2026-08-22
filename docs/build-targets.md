---
hide:
  - toc
---

## Build Targets

wakterm focuses build and test effort on three main platform environments: macOS, Linux (Ubuntu/Debian and Fedora), and Windows.

Upstream WezTerm's last tagged stable release is 20240203-110809 from February 2024. Stable package channels package that release, while several rolling distributions maintain Git snapshots.

| Platform / Format | WezTerm | wakterm | Notes |
|---|:-:|:-:|---|
| macOS (.zip, universal) | :material-check: | :material-check: | |
| macOS Homebrew cask | :material-check: | :material-check: | wakterm tap |
| macOS MacPorts | :material-check: | :material-close: | |
| Windows (setup.exe + zip) | :material-check: | :material-check: | |
| Windows winget | :material-check: | :material-check: | wakamex.wakterm |
| Windows Scoop | :material-check: | :material-close: | |
| Windows Chocolatey | :material-check: | :material-close: | |
| Ubuntu/Debian (.deb) | :material-check: | :material-check: | |
| Fedora (.rpm) | :material-check: | :material-check: | CI builds on fedora-latest |
| openSUSE | :material-check: | :material-close: | |
| Arch Linux (AUR) | :material-check: | :material-close: | |
| Alpine (apk) | :material-check: | :material-close: | |
| Flatpak (Flathub) | :material-check: | :material-close: | |
| AppImage | :material-check: | :material-close: | |
| Linuxbrew | :material-check: | :material-close: | |
| Nix / NixOS | :material-check: | :material-close: | flake in tree |
| FreeBSD | :material-check: | :material-close: | |
| NetBSD | :material-check: | :material-close: | |

### Platform focus

wakterm focuses build and test validation on macOS, Linux, and Windows. Packaging scripts for other targets remain in the source tree and can be enabled as packaging requirements expand.
