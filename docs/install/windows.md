## Installing on Windows

64-bit Windows 10 version 1809 (build 17763) or later is required to run wakterm, as it relies on [Windows Pseudo Console (ConPTY)](https://devblogs.microsoft.com/commandline/windows-command-line-introducing-the-windows-pseudo-console-conpty/) support.

### Scoop

If you use [Scoop](https://scoop.sh/), you can install wakterm from the `wakamex` bucket:

```console
$ scoop bucket add wakamex https://github.com/wakamex/scoop-bucket
$ scoop install wakterm
```

To upgrade to a newer release:

```console
$ scoop update wakterm
```

### Direct download

You can also download an [Inno Setup](https://jrsoftware.org/isinfo.php) installer (`setup.exe`) or a portable `.zip` archive from [GitHub Releases](https://github.com/wakamex/wakterm/releases).

[:fontawesome-brands-windows: Windows (setup.exe) :material-tray-arrow-down:]({{ windows_exe_stable }}){ .md-button }

The `setup.exe` installer installs wakterm to your Program Files directory and adds that location to `PATH`. It can run interactively or with the [standard Inno Setup command-line options](https://jrsoftware.org/ishelp/index.php?topic=setupcmdline).

[:fontawesome-brands-windows: Windows (zip) :material-tray-arrow-down:]({{ windows_zip_stable }}){ .md-button }

1. Download the [portable Windows zip archive]({{ windows_zip_stable }}).
2. Extract the archive and run `wakterm-gui.exe` or `wakterm.exe`.
3. Configuration instructions are in [Configuration Files](../guides/configuration/files.md).
