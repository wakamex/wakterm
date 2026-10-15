## Installing on macOS

wakterm ships as a Universal macOS binary supporting both Apple Silicon and Intel hardware.

## Homebrew

If you use [Homebrew](https://brew.sh/), you can install wakterm from the `wakamex/tap` tap:

```console
$ brew install --cask --no-quarantine wakamex/tap/wakterm
```

To upgrade to a newer release:

```console
$ brew upgrade --cask --no-quarantine wakamex/tap/wakterm
```

## Direct download

[:simple-apple: Download for macOS :material-tray-arrow-down:]({{ macos_zip_stable }}){ .md-button }

1. Download the [macOS release archive]({{ macos_zip_stable }}) from [GitHub Releases](https://github.com/wakamex/wakterm/releases).
2. Extract the `.zip` archive and drag `wakterm.app` to your `Applications` folder.
3. On first launch, right-click `wakterm.app` and select `Open` to allow launching the downloaded application.
4. To run `wakterm` commands from a shell, add the binary directory inside `wakterm.app` to your `PATH` in `~/.zshrc`:
```sh
PATH="$PATH:/Applications/wakterm.app/Contents/MacOS"
export PATH
```
5. Configuration instructions are in [Configuration Files](../guides/configuration/files.md).
