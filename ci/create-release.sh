#!/bin/bash
set -x
name="$1"

notes=$(cat <<EOT
See https://wakterm.org/changelog/#$name for the changelog.

Installation instructions:

- [Windows](https://wakterm.org/install/windows/)
- [macOS](https://wakterm.org/install/macos/)
- [Linux](https://wakterm.org/install/linux/)
- [Build from source](https://wakterm.org/install/source/)
EOT
)

gh release view "$name" || gh release create --notes "$notes" --title "$name" "$name"
