---
hide:
    - toc
---

=== "Fedora/RPM"
    ## Installing on Fedora and RPM-based systems

    The continuous integration (CI) release pipeline builds an `.rpm` package on Fedora that can be installed directly with `dnf` from [GitHub Releases](https://github.com/wakamex/wakterm/releases).

    [Download Fedora RPM :material-tray-arrow-down:]({{ fedora_rpm_stable }}){ .md-button }

    To install directly from the command line:

    ```console
    $ sudo dnf install -y {{ fedora_rpm_stable }}
    ```

    - The package installs `/usr/bin/wakterm`, `/usr/bin/wakterm-gui`, `/usr/bin/wakterm-mux-server`, and `/usr/share/applications/org.wezfurlong.wakterm.desktop`.
    - Configuration instructions are in [Configuration Files](../guides/configuration/files.md).

=== "Ubuntu/Debian"
    ## Installing on Ubuntu and Debian

    The CI release pipeline builds a `.deb` package on Ubuntu that can be installed with `apt` from [GitHub Releases](https://github.com/wakamex/wakterm/releases).

    [Download Ubuntu/Debian .deb :material-tray-arrow-down:]({{ ubuntu_deb_stable }}){ .md-button }

    To download and install from the command line:

    ```console
    $ curl -LO {{ ubuntu_deb_stable }}
    $ sudo apt install -y ./{{ ubuntu_deb_stable_asset }}
    ```

    - The package installs `/usr/bin/wakterm`, `/usr/bin/wakterm-gui`, `/usr/bin/wakterm-mux-server`, and `/usr/share/applications/org.wezfurlong.wakterm.desktop`.
    - Configuration instructions are in [Configuration Files](../guides/configuration/files.md).

=== "Raw"
    ## Raw Linux binary archive

    A portable `.tar.xz` archive containing the Linux binaries and shell integration files is also published on [GitHub Releases](https://github.com/wakamex/wakterm/releases).

    [Raw Linux Binary :material-tray-arrow-down:]({{ linux_raw_bin_stable }}){ .md-button }
