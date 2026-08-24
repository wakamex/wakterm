---
search:
  boost: 20
keywords: default keys key
tags:
 - keys
---

The default key assignments are shown in the table below.

You may also use `wakterm show-keys --lua` to see the assignments in a form that you can copy and paste into your own configuration.

| Modifiers | Key | Action |
| --------- | --- | ------ |
| `SUPER` | `c` | `CopyTo="Clipboard"` |
| `SUPER` | `v` | `PasteFrom="Clipboard"` |
| `CTRL+SHIFT` | `c` | `CopyTo="Clipboard"` |
| `CTRL+SHIFT` | `v` | `PasteFrom="Clipboard"` |
| | `Copy` | `CopyTo="Clipboard"` |
| | `Paste` | `PasteFrom="Clipboard"` |
| `CTRL` | `Insert` | `CopyTo="PrimarySelection"` |
| `SHIFT` | `Insert` | `PasteFrom="PrimarySelection"` |
| `SUPER` | `m` | `Hide` |
| `SUPER` | `n` | `SpawnWindow` |
| `CTRL+SHIFT` | `n` | `SpawnWindow` |
| `ALT` | `Enter` | `ToggleFullScreen` |
| `SUPER` | `-` | `DecreaseFontSize` |
| `CTRL` | `-` | `DecreaseFontSize` |
| `SUPER` | `=` | `IncreaseFontSize` |
| `CTRL` | `=` | `IncreaseFontSize` |
| `SUPER` | `0` | `ResetFontSize` |
| `CTRL` | `0` | `ResetFontSize` |
| `SUPER` | `t` | `SpawnTab="CurrentPaneDomain"` |
| `CTRL+SHIFT` | `t` | `SpawnTab="CurrentPaneDomain"` |
| `SUPER+SHIFT` | `T` | `SpawnTab="DefaultDomain"` |
| `SUPER` | `w` | `CloseCurrentTab{confirm=true}` |
| `CTRL+SHIFT` | `w` | `CloseCurrentTab{confirm=true}` |
| `SUPER` | `d` | `CloseCurrentPane{confirm=true}` |
| `CTRL+SHIFT` | `d` | `CloseCurrentPane{confirm=true}` |
| `SUPER+SHIFT` | `S` | `ParkCurrentTab` |
| `CTRL+SHIFT` | `S` | `ParkCurrentTab` |
| `SUPER` | `e` | `ShowTabNavigator` |
| `CTRL+SHIFT` | `E` | `ShowTabNavigator` |
| `SUPER` | `1` | `ActivateTab=0` |
| `SUPER` | `2` | `ActivateTab=1` |
| `SUPER` | `3` | `ActivateTab=2` |
| `SUPER` | `4` | `ActivateTab=3` |
| `SUPER` | `5` | `ActivateTab=4` |
| `SUPER` | `6` | `ActivateTab=5` |
| `SUPER` | `7` | `ActivateTab=6` |
| `SUPER` | `8` | `ActivateTab=7` |
| `SUPER` | `9` | `ActivateTab=-1` |
| `CTRL+SHIFT` | `1` | `ActivateTab=0` |
| `CTRL+SHIFT` | `2` | `ActivateTab=1` |
| `CTRL+SHIFT` | `3` | `ActivateTab=2` |
| `CTRL+SHIFT` | `4` | `ActivateTab=3` |
| `CTRL+SHIFT` | `5` | `ActivateTab=4` |
| `CTRL+SHIFT` | `6` | `ActivateTab=5` |
| `CTRL+SHIFT` | `7` | `ActivateTab=6` |
| `CTRL+SHIFT` | `8` | `ActivateTab=7` |
| `CTRL+SHIFT` | `9` | `ActivateTab=-1` |
| `SUPER+SHIFT` | `[` | `ActivateTabRelative=-1` |
| `CTRL+SHIFT` | `Tab` | `ActivateTabRelative=-1` |
| `CTRL` | `PageUp` | `ActivateTabRelative=-1` |
| `SUPER+SHIFT` | `]` | `ActivateTabRelative=1` |
| `CTRL` | `Tab` | `ActivateTabRelative=1` |
| `CTRL` | `PageDown` | `ActivateTabRelative=1` |
| `CTRL+SHIFT` | `PageUp` | `MoveTabRelative=-1` |
| `CTRL+SHIFT+ALT` | `[` | `MoveTabRelative=-1` |
| `OPT+CMD` | `[` | `MoveTabRelative=-1` (macOS only) |
| `CTRL+SHIFT` | `PageDown` | `MoveTabRelative=1` |
| `CTRL+SHIFT+ALT` | `]` | `MoveTabRelative=1` |
| `OPT+CMD` | `]` | `MoveTabRelative=1` (macOS only) |
| `SHIFT` | `PageUp` | `ScrollByPage=-1` |
| `SHIFT` | `PageDown` | `ScrollByPage=1` |
| `SHIFT` | `Home` | `ScrollToTop` |
| `SHIFT` | `End` | `ScrollToBottom` |
| `SUPER` | `r` | `ReloadConfiguration` |
| `CTRL+SHIFT` | `R` | `ReloadConfiguration` |
| `SUPER` | `h` | `HideApplication` (macOS only) |
| `SUPER` | `k` | `ClearScrollback="ScrollbackOnly"` |
| `CTRL+SHIFT` | `K` | `ClearScrollback="ScrollbackOnly"` |
| `CTRL+SHIFT` | `L` | `ShowDebugOverlay` |
| `CTRL+SHIFT` | `P` | `ActivateCommandPalette` |
| `SUPER` | `<` | `PromptRenameTab` |
| `CTRL+SHIFT` | `<` | `PromptRenameTab` |
| `SUPER` | `o` | `RotatePanes="Clockwise"` |
| `CTRL+SHIFT` | `O` | `RotatePanes="Clockwise"` |
| `CTRL+SHIFT` | `U` | `CharSelect` |
| `SUPER` | `f` | `Search={CaseSensitiveString=""}` |
| `CTRL+SHIFT` | `F` | `Search={CaseSensitiveString=""}` |
| `CTRL+SHIFT` | `X` | `ActivateCopyMode` |
| `CTRL+SHIFT` | `Space` | `QuickSelect` |
| `CTRL+SHIFT` | `M` | `PaneSelect={mode="SwapWithActive"}` |
| `CTRL+SHIFT+ALT` | `"` | `SplitVertical={domain="CurrentPaneDomain"}` |
| `CTRL+SHIFT+ALT` | `%` | `SplitHorizontal={domain="CurrentPaneDomain"}` |
| `CTRL+SHIFT+ALT` | `LeftArrow` | `AdjustPaneSize={"Left", 1}` |
| `CTRL+SHIFT+ALT` | `RightArrow` | `AdjustPaneSize={"Right", 1}` |
| `CTRL+SHIFT+ALT` | `UpArrow` | `AdjustPaneSize={"Up", 1}` |
| `CTRL+SHIFT+ALT` | `DownArrow` | `AdjustPaneSize={"Down", 1}` |
| `CTRL+SHIFT` | `LeftArrow` | `ActivatePaneDirection="Left"` |
| `CTRL+SHIFT` | `RightArrow` | `ActivatePaneDirection="Right"` |
| `CTRL+SHIFT` | `UpArrow` | `ActivatePaneDirection="Up"` |
| `CTRL+SHIFT` | `DownArrow` | `ActivatePaneDirection="Down"` |
| `SUPER+ALT` | `LeftArrow` | `ActivatePaneDirection="Left"` (macOS only) |
| `SUPER+ALT` | `RightArrow` | `ActivatePaneDirection="Right"` (macOS only) |
| `SUPER+ALT` | `UpArrow` | `ActivatePaneDirection="Up"` (macOS only) |
| `SUPER+ALT` | `DownArrow` | `ActivatePaneDirection="Down"` (macOS only) |
| `CTRL+SHIFT` | `Z` | `TogglePaneZoomState` |

If you do not want the default assignments to be registered, you can disable all of them with this configuration:

```lua
config.disable_default_key_bindings = true
```

When using `disable_default_key_bindings`, it is recommended that you assign [ShowDebugOverlay](../../reference/lua/keyassignment/ShowDebugOverlay.md) and [ActivateCommandPalette](../../reference/lua/keyassignment/ActivateCommandPalette.md) to custom shortcuts for troubleshooting.
