#!/bin/bash
set -x
name="$1"

notes=$(cat <<EOT
See https://wakterm.org/changelog.html#$name for the changelog

If you're looking for nightly downloads or more detailed installation instructions:

[Windows](https://wakterm.org/install/windows.html)
[macOS](https://wakterm.org/install/macos.html)
[Linux](https://wakterm.org/install/linux.html)
[FreeBSD](https://wakterm.org/install/freebsd.html)
EOT
)

gh release view "$name" || gh release create --prerelease --notes "$notes" --title "$name" "$name"
