# Zola documentation migration

Status: Milestones 3 through 6 complete on `main`.

## Content and routes

- The build stages 655 source documents into 626 pages and 30 sections.
- Task guides, Lua Config fields, Lua API reference, CLI reference, CLI help fragments, and key
  tables now have separate source trees.
- `docs-site/route-map.tsv` records 621 information-architecture route changes.
- `docs-site/legacy-redirects.tsv` preserves all 655 former public page URLs. Every redirect target
  exists in the built route inventory.
- Three generated but unused CLI help fragments were removed. The remaining derived files still use
  Wakterm's existing update and stale-output checks.
- Includes, release variables, tabs, admonitions, Material icons, relative Markdown links, and six
  Mermaid blocks have deterministic build-time conversions. Mermaid remains an optional theme
  feature and is loaded only on pages containing a diagram.

## Interface

Wakterm vendors reusable Zola Docs theme commit
`90b7f25671ca33c8a018a408a7ef1eb08b238206`. The theme supplies responsive navigation,
breadcrumbs, optional page navigation and metadata, edit links, heading anchors, appearance
selection, accessible search, code copying, conditional Mermaid, 404 guidance, and optional
route-preserving documentation version selection. Wakterm starts with versioning disabled.

The theme test suite passes with official Zola 0.23.4. The complete Wakterm corpus also builds at
both `https://wakterm.org/` and the test base path `https://wakterm.org/docs/1.0/` without
root-relative links.

## Audit and comparison

Measured on 2026-08-24 on the same Fedora development host as the MkDocs baseline.

| Case | MkDocs Material | Zola 0.23.4 |
| --- | ---: | ---: |
| Warm complete local pipeline | 25.95 s | 1.47 to 1.54 s |
| One-page development rebuild | 24.80 s | 0.229 to 0.235 s |
| Generator build only | included above | 0.299 to 0.307 s |
| Output files | 1,447 | 674 |
| HTML routes | 656 | 658 |
| Total output | 245,993,959 bytes | 100,125,366 bytes |
| HTML | 172,746,471 bytes | 96,063,712 bytes |
| Search index | 1,404,389 bytes | 656,861 bytes |

The complete Zola pipeline is about 16 times faster than the warm MkDocs build. Its one-page
rebuild is about 105 times faster. Total output is 59 percent smaller and HTML is 44 percent
smaller. The two extra HTML routes are explicit section landing pages introduced by the clean
information architecture.

The built search index contains 657 records. The expected page ranks first for `restore layout`,
`download linux`, `quick select`, `agent harness`, and `ShowTabNavigator`. Zola's strict check
reports no broken internal links or orphan pages. The publication manifest contains no accidental
MkDocs internals.

## Reproducibility and cutover

- `ci/install-zola.sh` pins official Zola 0.23.4 and verifies the Linux x86_64 release archive with
  SHA-256 `54d1a347781b2f32330914fcc02def81c7e3ddb6111b36d1cc89c06557aed1de`.
- `ci/build-docs.sh` retains the Lua formatting gate, uses fetched release metadata when available,
  falls back offline, prepares the corpus, builds it, and runs strict link checking.
- The Cloudflare Pages production and pull-request workflows use the same build command.
- MkDocs, its container image, macros, overrides, and copy workaround have been removed.
- Backup branch `backup/mkdocs-before-zola-cutover-20260824` points to
  `78d6b8cbbec7c702f7d1abcf94acf37bc90dd58b` and contains the complete former MkDocs pipeline.
- GitHub Actions run `32694066266` built and deployed the site successfully. Live route, search,
  conditional Mermaid, interface-control, and legacy redirect checks passed after deployment.

Build with `make docs`. Use `DOCS_OFFLINE=1 make docs` to force fallback release links without a
network request. Run the authoring server with `make servedocs`.
