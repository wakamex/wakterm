# Tab color palette generator

This offline tool exhaustively evaluates the 24-bit sRGB color space and emits
the Dark, Light, and Mixed tab color sequences used by the GUI.

For each scheme, it filters RGB colors by the configured Oklab lightness and
chroma bounds. It measures squared Oklab distance after the default 0.4 inactive
rendering intensity, then repeatedly selects the color farthest from its nearest
selected color. Each scheme is ranked independently from its complete eligible
RGB8 domain.

Generate the checked-in 512-color sequences from the repository root:

```sh
cargo run -p generate-tab-color-palettes --release -- --count 512
```

Verify that the checked-in file is reproducible:

```sh
cargo run -p generate-tab-color-palettes --release -- --count 512 --check
```

Add another scheme by defining its bounds in `src/main.rs`. Each scheme is
ranked independently because filtering one global sequence would not preserve
farthest-first spacing within the filtered domain.
