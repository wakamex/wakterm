## Installing on macOS

The CI system builds the package on macOS Big Sur and should run on systems as
"old" as Mojave.  It may run on earlier versions of macOS, but that has not
been tested.

Starting with version 20210203-095643-70a364eb, wakterm is a Universal binary
with support for both Apple Silicon and Intel hardware.

[:simple-apple: Download for macOS :material-tray-arrow-down:]({{ macos_zip_stable }}){ .md-button }
[:simple-apple: Nightly for macOS :material-tray-arrow-down:]({{ macos_zip_nightly }}){ .md-button }

1. Download <a href="{{ macos_zip_stable }}">Release</a>.
2. Extract the zipfile and drag the `wakterm.app` bundle to your `Applications` folder.
3. First time around, you may need to right click and select `Open` to allow launching
   the application that you've just downloaded from the internet.
3. Subsequently, a simple double-click will launch the UI.
4. To use wakterm binary from a terminal emulator, like `wakterm ls-fonts` you'll need to add the location to the wakterm binary folder that exists _inside_ the wakterm.app, to your environment's $PATH value. For example, to add it to your `~/.zshrc` file, and assuming your wakterm.app was installed to `/Applications`, add:
```sh
PATH="$PATH:/Applications/wakterm.app/Contents/MacOS"
export PATH
```
5. Configuration instructions can be [found here](../config/files.md)

## Homebrew

wakterm is available for [brew](https://brew.sh/) users:

```console
$ brew install --cask wakterm
```

If you'd like to use a nightly build:

```console
$ brew install --cask wakterm@nightly
```

!!! note
    For users who have previously used the cask named `wakterm-nightly`,
    homebrew has started issuing warnings: `Warning: Cask
    homebrew/cask-versions/wakterm-nightly was renamed to wakterm@nightly`. We
    recommend that you use `brew uninstall wakterm-nightly` to uninstall the
    previously installed version, and then reinstall the new version using the
    command above.

to upgrade to a newer nightly (normal `brew upgrade` will not upgrade it!):

```console
$ brew upgrade --cask wakterm@nightly --no-quarantine --greedy-latest
```

!!! note
    The `--greedy-latest` option in Homebrew forces the latest version of a
    formula to be installed, even if a version satisfying the formula's
    requirements is already installed. This can be useful when you want to
    ensure you have the most up-to-date version of a package, regardless of
    whether an older version meets the current dependency requirements.

## MacPorts

wakterm is also available via [MacPorts](https://ports.macports.org/port/wakterm/summary):

```console
$ sudo port selfupdate
$ sudo port install wakterm
```

