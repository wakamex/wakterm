# Tab Bar Agent Icons

## Goal

Show a small harness-specific icon beside the tab title for tabs whose active
pane is an agent pane.

Initial targets:

- Agy
- Claude
- Codex
- Gemini
- OpenCode

This is a tab bar feature, not a tab title string feature.

## Current Recommendation

There are now two realistic implementation paths:

- cached hardcoded vector sprites
- packaged image assets (PNG or build-time-rasterized SVG)

For the current product shape, where we only need five fixed icons, the
recommended first implementation is:

- fancy tab bar only
- active-pane harness only
- hardcoded vector icon definitions for the five supported harnesses
- rasterize each icon once into the existing atlas as a cached sprite
- render the cached sprite beside the tab title

This is intentionally **not** generic SVG support.

### Why This Is The Current Recommendation

The GUI already has two relevant rendering paths:

- `ElementContent::Poly` in
  `wakterm-gui/src/termwindow/box_model.rs`, which renders vector/poly content
  directly during UI rendering
- cached vector-to-sprite generation in
  `wakterm-gui/src/customglyph.rs`, which rasterizes vector definitions into the
  atlas and then reuses the sprite

For tab icons, the second path is better:

- one-time rasterization
- cheap textured-quad rendering afterwards
- no runtime SVG parsing
- no arbitrary user/content API surface
- no dependency on fonts or Unicode glyph availability

So if we implement custom vector icons, they should be treated as **cached icon
sprites**, not as directly rendered per-frame polylines.

## Why This Shape

The existing tab title pipeline is text-oriented:

- `wakterm-gui/src/termwindow/mod.rs` stores `TabInformation.tab_title` as a
  `String`
- `wakterm-gui/src/tabbar.rs` formats tab titles as text/`FormatItem`s
- image escape actions are ignored by the tab title parser

The fancy tab bar already uses the richer box-model render path and can render
sprites directly, so the icon should be implemented there instead of trying to
embed SVG or image data into the title string.

## Current Code Paths

Relevant files:

- `wakterm-gui/src/termwindow/mod.rs`
  - `TabInformation`
  - `get_tab_information()`
- `wakterm-gui/src/tabbar.rs`
  - `TabEntry`
  - `compute_tab_title()`
- `wakterm-gui/src/termwindow/render/fancy_tab_bar.rs`
  - fancy tab bar element construction
- `wakterm-gui/src/termwindow/box_model.rs`
  - sprite-capable box-model rendering
- `wakterm-gui/src/customglyph.rs`
  - cached vector-to-sprite rasterization helpers
- `wakterm-gui/src/glyphcache.rs`
  - atlas-backed image/sprite caching
- `mux/src/agent.rs`
  - `AgentHarness`
- `mux/src/lib.rs`
  - agent metadata/runtime lookup by pane

## Data Model Plan

Add a lightweight icon hint to the GUI tab model.

### `TabInformation`

Extend `TabInformation` with something like:

```rust
pub harness_icon: Option<TabHarnessIcon>
```

Where `TabHarnessIcon` is a small GUI enum, for example:

```rust
enum TabHarnessIcon {
    Agy,
    Claude,
    Codex,
    Gemini,
    OpenCode,
}
```

This should also be exposed to Lua so `format-tab-title` can react to it if
needed, but the actual icon drawing should remain a native GUI responsibility.

### Source of Truth

Populate the icon hint from the active pane of the tab in
`TermWindow::get_tab_information()`.

Do not call `list_agents()` from the tab bar or paint path.

Instead, add or reuse a cheap cached mux getter that can answer:

- whether a pane is an agent pane
- which harness it belongs to

This lookup must only consult already-cached adopted/detected agent state and
must not trigger synchronous harness refresh work.

## Icon Source Options

### Option A: Hardcoded Vector Icons

Define four internal icon shapes in code:

- Claude
- Codex
- Gemini
- OpenCode

These should be simplified, compact harness marks rather than full wordmarks.
They should be rendered into RGBA or monochrome sprites once and cached in the
atlas, similar in spirit to the custom cursor/block glyph path.

#### Pros

- fastest runtime path after caching
- no new SVG parser or runtime rasterizer
- no dependency on asset decode in the hot path
- tightly controlled sizing and styling
- minimal product/API surface

#### Cons

- lower brand fidelity than official logos
- requires hand-authoring/maintaining the vector shapes
- monochrome/tintable paths are easier than full-color logos

### Icon Authoring Workflow

The implementation is runtime-hardcoded, but the authoring process is:

1. start from an official asset when possible
2. if needed, trace or flatten it into a monochrome polygon-like SVG
3. simplify candidate outlines and render a review sheet
4. choose the highest-fidelity version whose point count is already modest
5. hand-convert the chosen shape into `PolyCommand` data in
   `wakterm-gui/src/customglyph.rs`
6. build and inspect the real fancy tab bar before shipping

The saved helper for step 3 is:

- [`scripts/icon_simplify_review.py`](/code/wakterm/scripts/icon_simplify_review.py)

It is intentionally narrow:

- input must already be polygon-like SVG path data
- outline simplification uses Ramer-Douglas-Peucker
- supported path commands are `M`, `L`, `H`, `V`, `Z`
- no cubic curves, transforms, gradients, or generic SVG support

That matches the problem we actually solved. Trace output is a bootstrap tool,
not the final asset.

Simplification is optional. If the original or traced source already reads best
at tab size, ship that instead of forcing a lower-complexity version.

Treat point count as a sufficiency budget, not an objective to minimize. Compare
the absolute point count, number of subpaths, and shape type before accepting a
more aggressive simplification. Curved marks need enough closely spaced points
to avoid visible facets. Once a candidate is already comparable in complexity
to the existing icons, prefer the smoother result unless removing more points
has a concrete rendering or maintenance benefit. Review candidates at the
actual small render sizes as well as on the large review sheet.

Example usage:

```bash
python scripts/icon_simplify_review.py \
  --svg /tmp/icon-source-stripped.svg \
  --out-dir /tmp/icon-simplify \
  --sheet /tmp/icon-simplify-review.png \
  --compare "Bootstrap candidate|Traced or flattened source|/tmp/icon-bootstrap.png" \
  --candidate 3.0:"Simplified A" \
  --candidate 4.0:"Simplified B"
```

This generates one review PNG containing the official/source card and the
simplified candidates for side-by-side comparison.

Dependencies:

- Python 3
- Pillow
- ImageMagick `magick`

### Option B: Packaged Image Assets

Use packaged image assets, not runtime generic SVG parsing.

Reasons:

- simpler implementation
- no generic SVG dependency in the render path
- the GUI already has PNG/image decoding and sprite atlas plumbing

Suggested asset location for this path:

- `assets/tab-icons/`

Suggested files:

- `claude.png`
- `codex.png`
- `gemini.png`
- `opencode.png`

#### Anthropic / Claude

For Claude, prefer the official icon asset from the local Anthropic brand kit:

- `/home/mihai/defi/anthropic_brand_kit.zip`
- `Anthropic media resources/Anthropic logos/Claude logos/4 Claude icon/SVG/ClaudeIcon-Rounded.svg`

For a small tab badge, use the icon asset, not the `Claude Code` wordmark.

#### Branding / Shipping

For local experimentation, official assets are fine.

For public/upstream distribution, brand/trademark review should remain separate
from the implementation. If needed, start with internal placeholder/custom
assets and swap to official assets later.

### Why Not Generic SVG Support

Generic SVG-in-tab-bar is a much larger feature than the product needs.

It would introduce:

- a parsing/rasterization surface for arbitrary content
- more layout and scaling edge cases
- more caching invalidation concerns
- a larger API than “show one of four known harness icons”

We do not need that flexibility. The product requirement is narrow and known in
advance, so the implementation should stay narrow too.

## Rendering Plan

### V1 Scope

Render icons only in the fancy tab bar.

Do not change the classic text-only tab bar in the first iteration.

### Recommended V1 Scope

Use cached hardcoded vector sprites in the fancy tab bar only.

Keep the packaged-image path as a fallback if we later decide exact official
brand fidelity matters more than the narrower implementation.

### `TabEntry`

Extend `TabEntry` in `wakterm-gui/src/tabbar.rs` with an optional icon field,
for example:

```rust
pub icon: Option<TabHarnessIcon>
```

Also reserve a fixed width for the icon when computing tab width and
truncation.

### Fancy Tab Bar

In `wakterm-gui/src/termwindow/render/fancy_tab_bar.rs`:

- resolve the icon into a cached sprite
- place it before the title text
- use a fixed compact size suitable for tabs
- keep spacing consistent whether the icon is present or absent

The box-model/sprite path already exists, so this should be implemented as a
sprite element rather than as terminal text cells.

### Cached Vector Sprite Plan

If using hardcoded vector icons:

- add a small icon enum, e.g. `TabHarnessIcon`
- add a cache keyed by `(icon kind, scale bucket, theme variant if needed)`
- rasterize the icon to an RGBA image once
- allocate it into the atlas
- reuse the resulting sprite during fancy-tab rendering

This is preferable to rendering the vector path every frame via
`ElementContent::Poly`.

### Fallback Behavior

If any of the following is true:

- no active pane
- active pane is not an agent
- no known harness
- asset missing
- tab is too narrow

then omit the icon and render the tab title normally.

## Caching Plan

Cache icon sprites by:

- icon kind
- scale / DPI bucket
- theme variant if needed

Do not decode or rasterize assets on every repaint.

Use the existing GUI image/sprite atlas infrastructure.

## Behavior Rules

- The icon reflects the active pane in the tab.
- Switching active panes in a split tab should update the icon.
- Reconnect/attach should restore the icon without requiring special user
  action.
- Window title behavior is unchanged.
- Pane title behavior is unchanged.

## Phasing

### Phase 1

- add `TabHarnessIcon`
- plumb icon hint into `TabInformation`
- render cached icon sprites in fancy tab bar only

### Phase 2

- optionally swap custom vector sprites for packaged official assets
- add classic tab bar support if desired

### Phase 3

- if needed, support build-time rasterization from checked-in SVG source
  while keeping runtime rendering sprite-based

### Phase 4

- optional Lua customization hooks for hiding/replacing icons

## Non-Goals

- inline SVG markup in tab titles
- arbitrary user-supplied SVG rendering in the title string path or tab bar
- changing tab title strings themselves
- synchronous agent discovery/refresh in the paint path

## Open Questions

- Should the icon represent the active pane only, or should a tab with any
  agent pane show an icon even when a non-agent split is active?
- Should Claude use the generic Claude icon or a distinct Claude Code icon when
  the harness is specifically `Claude Code`?
- Do we want a config knob to disable icons globally?
- Do we want a Lua override for per-harness icon selection later?

## Done When

- Tabs with active Agy/Claude/Codex/Gemini/OpenCode panes show a small icon beside
  the title in the fancy tab bar.
- Switching panes in a split tab updates the icon correctly.
- Reconnect/attach restores icons correctly.
- Missing assets fail soft and do not break tab rendering.
- No tab bar render path performs synchronous agent refresh or `list_agents()`
  work.
