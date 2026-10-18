#!/usr/bin/env bash
set -euo pipefail

TAG_NAME=${TAG_NAME:-"v$(cat VERSION)"}
VERSION_NO_V=${TAG_NAME#v}
OUT_DIR=${1:-.}

PYTHON_BIN=$(command -v python3 || command -v python)
XCB_MANIFEST=$("$PYTHON_BIN" -c '
import json, subprocess
meta = json.loads(subprocess.check_output(["cargo", "metadata", "--format-version", "1", "--locked"]))
for pkg in meta["packages"]:
    if pkg["name"] == "xcb-imdkit":
        print(pkg["manifest_path"])
        break
')

if [[ -z "$XCB_MANIFEST" || ! -f "$XCB_MANIFEST" ]]; then
  echo "Error: could not locate xcb-imdkit in cargo metadata" >&2
  exit 1
fi

XCB_DIR=$(dirname "$XCB_MANIFEST")
ARCHIVE_BASE="wakterm-${TAG_NAME}-xcb-imdkit-source"
WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT

STAGE_DIR="$WORK_DIR/$ARCHIVE_BASE"
mkdir -p "$STAGE_DIR/xcb-imdkit-rs"

tar -C "$XCB_DIR" --exclude='.git' --exclude='.cargo-ok' -cf - . | tar -C "$STAGE_DIR/xcb-imdkit-rs" -xf -
cp licenses/COPYING.LGPLv2.1 "$STAGE_DIR/COPYING.LGPLv2.1"

cat > "$STAGE_DIR/README.txt" <<EOF
wakterm ${VERSION_NO_V} on Linux (X11) statically links xcb-imdkit 0.3.0
(https://github.com/wakamex/xcb-imdkit-rs, commit 212330f7c6c37794d78061a773e8f4f9e4785bbb)
under the GNU Lesser General Public License version 2.1 (COPYING.LGPLv2.1 in this archive).

xcb-imdkit-rs/ contains the complete source code and build scripts (build.rs, C sources,
headers, and bundled xcb-imdkit/uthash sources) used to compile the crate.

To relink wakterm against a modified copy of xcb-imdkit:
1. Check out https://github.com/wakamex/wakterm at tag ${TAG_NAME}.
2. Point the xcb-imdkit dependency in Cargo.toml (or a [patch."https://github.com/wakamex/xcb-imdkit-rs.git"]
   section in .cargo/config.toml) to your modified xcb-imdkit-rs directory.
3. Run: cargo build --release -p wakterm -p wakterm-gui -p wakterm-mux-server -p strip-ansi-escapes
EOF

OUT_FILE="${OUT_DIR}/${ARCHIVE_BASE}.tar.gz"
tar -C "$WORK_DIR" -czf "$OUT_FILE" "$ARCHIVE_BASE"
echo "Created $OUT_FILE"
