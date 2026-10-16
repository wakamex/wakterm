# wakterm (wakamex fork)

This is an actively maintained fork of [wezterm/wezterm](https://github.com/wezterm/wezterm), extending it with fixes and features for daily mux server usage. I also took the opportunity to make agent harnesses first-class citizens 🤖.

Current features and documentation: [wakterm.org](https://wakterm.org/)

Historical fork notes: [CHANGELOG-FORK.md](CHANGELOG-FORK.md)

---

## Installation

See [wakterm.org/installation](https://wakterm.org/installation) for Linux (`.rpm`, `.deb`, `.tar.xz`), macOS ([Homebrew](https://brew.sh/) tap `wakamex/tap` and `.zip`), Windows ([Scoop](https://scoop.sh/) bucket `wakamex/scoop-bucket`, `setup.exe`, and `.zip`), and building from source.

### Verifying a release download

Every asset on [GitHub Releases](https://github.com/wakamex/wakterm/releases) is published with `SHA256SUMS` and a build provenance attestation. Verify a downloaded file with [GitHub CLI](https://cli.github.com/):

```sh
gh attestation verify ASSET --repo wakamex/wakterm \
  --signer-workflow wakamex/release-actions/.github/workflows/binary-release.yml
gh release verify-asset vX.Y.Z ASSET --repo wakamex/wakterm
```

## License

wakterm's source code is distributed under the MIT license in [LICENSE.md](LICENSE.md). The bundled fonts in `assets/fonts/` are distributed under the [SIL Open Font License (OFL) 1.1](https://openfontlicense.org/), the MIT license, the Apache License 2.0, [Creative Commons Attribution 4.0 (CC BY 4.0)](https://creativecommons.org/licenses/by/4.0/), and [The Unlicense](https://unlicense.org/), as detailed in [LICENSE.md](LICENSE.md) and `assets/fonts/LICENSE_*.txt`.

## Getting help

If you find any issues with this fork, file a GitHub issue.

## Supporting the project

If you use and like wakterm, consider [supporting WezTerm](https://wezterm.org/sponsor.html), the project it was built on.
