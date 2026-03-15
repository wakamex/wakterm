# WezTerm Hotkeys Reference

Auto-generated from `wezterm show-keys`. Run `python3 generate-hotkeys.py > HOTKEYS.md` to update.

## Default Key Bindings

| Key | Action | All Bindings | Upstream |
|-----|--------|-------------|----------|
| $ | `CopyMode(MoveToEndOfLineContent)` | $, End, Shift+$ | same |
| , | `CopyMode(JumpReverse)` | , | same |
| 0 | `CopyMode(MoveToStartOfLine)` | 0, Home | same |
| ; | `CopyMode(JumpAgain)` | ; | same |
| Alt+Ctrl+" | `SplitVertical(SpawnCommand domain=CurrentPaneDomain)` | Alt+Ctrl+", Shift+Alt+Ctrl+", Shift+Alt+Ctrl+' | same |
| Alt+Ctrl+% | `SplitHorizontal(SpawnCommand domain=CurrentPaneDomain)` | Alt+Ctrl+%, Shift+Alt+Ctrl+%, Shift+Alt+Ctrl+5 | same |
| Alt+Enter | `ToggleFullScreen` | Alt+Enter | same |
| Copy | `CopyTo(Clipboard)` | Copy, Ctrl+C, Shift+Ctrl+C | same |
| Ctrl+! | `ActivateTab(0)` | Ctrl+!, Shift+Ctrl+!, Shift+Ctrl+1 | same |
| Ctrl+# | `ActivateTab(2)` | Ctrl+#, Shift+Ctrl+#, Shift+Ctrl+3 | same |
| Ctrl+$ | `ActivateTab(3)` | Ctrl+$, Shift+Ctrl+$, Shift+Ctrl+4 | same |
| Ctrl+% | `ActivateTab(4)` | Ctrl+%, Shift+Ctrl+%, Shift+Ctrl+5 | same |
| Ctrl+& | `ActivateTab(6)` | Ctrl+&, Shift+Ctrl+&, Shift+Ctrl+7 | same |
| Ctrl+( | `ActivateTab(-1)` | Ctrl+(, Shift+Ctrl+(, Shift+Ctrl+9 | same |
| Ctrl+) | `ResetFontSize` | Ctrl+), Ctrl+0, Shift+Ctrl+) | same |
| Ctrl+* | `ActivateTab(7)` | Ctrl+*, Shift+Ctrl+*, Shift+Ctrl+8 | same |
| Ctrl++ | `IncreaseFontSize` | Ctrl++, Ctrl+=, Shift+Ctrl++ | same |
| Ctrl+- | `DecreaseFontSize` | Ctrl+-, Ctrl+_, Shift+Ctrl+- | same |
| Ctrl+@ | `ActivateTab(1)` | Ctrl+@, Shift+Ctrl+2, Shift+Ctrl+@ | same |
| Ctrl+F | `Search(CurrentSelectionOrEmptyString)` | Ctrl+F, Shift+Ctrl+F, Shift+Ctrl+f | same |
| Ctrl+Insert | `CopyTo(PrimarySelection)` | Ctrl+Insert | same |
| Ctrl+K | `ClearScrollback(ScrollbackOnly)` | Ctrl+K, Shift+Ctrl+K, Shift+Ctrl+k | same |
| Ctrl+L | `ShowDebugOverlay` | Ctrl+L, Shift+Ctrl+L, Shift+Ctrl+l | same |
| Ctrl+M | `Hide` | Ctrl+M, Shift+Ctrl+M, Shift+Ctrl+m | same |
| Ctrl+N | `SpawnWindow` | Ctrl+N, Shift+Ctrl+N, Shift+Ctrl+n | same |
| Ctrl+P | `ActivateCommandPalette` | Ctrl+P, Shift+Ctrl+P, Shift+Ctrl+p | same |
| Ctrl+R | `ReloadConfiguration` | Ctrl+R, Shift+Ctrl+R, Shift+Ctrl+r | same |
| Ctrl+T | `SpawnTab(CurrentPaneDomain)` | Ctrl+T, Shift+Ctrl+T, Shift+Ctrl+t | same |
| Ctrl+U | `CharSelect(CharSelectArguments { group: None, copy_on_sel...` | Ctrl+U, Shift+Ctrl+U, Shift+Ctrl+u | same |
| Ctrl+W | `CloseCurrentTab { confirm: true }` | Ctrl+W, Shift+Ctrl+W, Shift+Ctrl+w | same |
| Ctrl+X | `ActivateCopyMode` | Ctrl+X, Shift+Ctrl+X, Shift+Ctrl+x | same |
| Ctrl+Z | `TogglePaneZoomState` | Ctrl+Z, Shift+Ctrl+Z, Shift+Ctrl+z | same |
| Ctrl+^ | `ActivateTab(5)` | Ctrl+^, Shift+Ctrl+6, Shift+Ctrl+^ | same |
| Ctrl+b | `CopyMode(PageUp)` | Ctrl+b, PageUp | same |
| Ctrl+d | `CopyMode(MoveByPage(0.5))` | Ctrl+d | same |
| Ctrl+f | `CopyMode(PageDown)` | Ctrl+f, PageDown | same |
| Ctrl+n | `CopyMode(NextMatch)` | Ctrl+n, DownArrow | same |
| Ctrl+r | `CopyMode(CycleMatchType)` | Ctrl+r | same |
| Ctrl+u | `CopyMode(ClearPattern)` | Ctrl+u | same |
| Ctrl+u | `CopyMode(MoveByPage(-0.5))` | Ctrl+u | same |
| Ctrl+v | `CopyMode(SetSelectionMode(Some(Block)))` | Ctrl+v | same |
| Enter | `CopyMode(MoveToStartOfNextLine)` | Enter | same |
| Enter | `CopyMode(PriorMatch)` | Ctrl+p, Enter, UpArrow | same |
| Escape | `CopyMode(Close)` | Escape | same |
| F | `CopyMode(JumpBackward { prev_char: false })` | F, Shift+F | same |
| G | `CopyMode(MoveToScrollbackBottom)` | G, Shift+G | same |
| H | `CopyMode(MoveToViewportTop)` | H, Shift+H | same |
| L | `CopyMode(MoveToViewportBottom)` | L, Shift+L | same |
| M | `CopyMode(MoveToViewportMiddle)` | M, Shift+M | same |
| O | `CopyMode(MoveToSelectionOtherEndHoriz)` | O, Shift+O | same |
| PageDown | `CopyMode(NextMatchPage)` | PageDown | same |
| PageUp | `CopyMode(PriorMatchPage)` | PageUp | same |
| Paste | `PasteFrom(Clipboard)` | Ctrl+V, Paste, Shift+Ctrl+V | same |
| Shift+Alt+Ctrl+DownArrow | `AdjustPaneSize(Down, 1)` | Shift+Alt+Ctrl+DownArrow | same |
| Shift+Alt+Ctrl+LeftArrow | `AdjustPaneSize(Left, 1)` | Shift+Alt+Ctrl+LeftArrow | same |
| Shift+Alt+Ctrl+RightArrow | `AdjustPaneSize(Right, 1)` | Shift+Alt+Ctrl+RightArrow | same |
| Shift+Alt+Ctrl+UpArrow | `AdjustPaneSize(Up, 1)` | Shift+Alt+Ctrl+UpArrow | same |
| Shift+Ctrl+DownArrow | `ActivatePaneDirection(Down)` | Shift+Ctrl+DownArrow | same |
| Shift+Ctrl+LeftArrow | `ActivatePaneDirection(Left)` | Shift+Ctrl+LeftArrow | same |
| Shift+Ctrl+PageDown | `MoveTabRelative(1)` | Shift+Ctrl+PageDown | same |
| Shift+Ctrl+PageUp | `MoveTabRelative(-1)` | Shift+Ctrl+PageUp | same |
| Shift+Ctrl+RightArrow | `ActivatePaneDirection(Right)` | Shift+Ctrl+RightArrow | same |
| Shift+Ctrl+UpArrow | `ActivatePaneDirection(Up)` | Shift+Ctrl+UpArrow | same |
| Shift+Insert | `PasteFrom(PrimarySelection)` | Shift+Insert | same |
| Shift+PageDown | `ScrollByPage(1.0)` | Shift+PageDown | same |
| Shift+PageUp | `ScrollByPage(-1.0)` | Shift+PageUp | same |
| Super+{ | `ActivateTabRelative(-1)` | Ctrl+PageUp, Shift+Ctrl+Tab, Shift+Super+[ | same |
| Super+} | `ActivateTabRelative(1)` | Ctrl+PageDown, Ctrl+Tab, Shift+Super+] | same |
| T | `CopyMode(JumpBackward { prev_char: true })` | Shift+T, T | same |
| V | `CopyMode(SetSelectionMode(Some(Line)))` | Shift+V, V | same |
| ^ | `CopyMode(MoveToStartOfLineContent)` | Alt+m, Shift+^, ^ | same |
| b | `CopyMode(MoveBackwardWord)` | Alt+LeftArrow, Alt+b, Shift+Tab | same |
| e | `CopyMode(MoveForwardWordEnd)` | e | same |
| f | `CopyMode(JumpForward { prev_char: false })` | f | same |
| g | `CopyMode(MoveToScrollbackTop)` | g | same |
| h | `CopyMode(MoveLeft)` | LeftArrow, h | same |
| j | `CopyMode(MoveDown)` | DownArrow, j | same |
| k | `CopyMode(MoveUp)` | UpArrow, k | same |
| l | `CopyMode(MoveRight)` | RightArrow, l | same |
| o | `CopyMode(MoveToSelectionOtherEnd)` | o | same |
| q | `Multiple([ScrollToBottom, CopyMode(Close)])` | Ctrl+c, Ctrl+g, Escape | same |
| t | `CopyMode(JumpForward { prev_char: true })` | t | same |
| v | `CopyMode(SetSelectionMode(Some(Cell)))` | Space, v | same |
| w | `CopyMode(MoveForwardWord)` | Alt+RightArrow, Alt+f, Tab | same |
| y | `Multiple([CopyTo(ClipboardAndPrimarySelection), Multiple(...` | y | same |

*85 bindings in the default key table.*

## Assignable Actions Without Default Bindings

These can be bound via `config.keys` in your wezterm config.

- `ActivateKeyTable`
- `ActivateLastTab`
- `ActivatePaneByIndex`
- `ActivateTabRelativeNoWrap`
- `ActivateWindow`
- `ActivateWindowRelative`
- `ActivateWindowRelativeNoWrap`
- `AttachDomain`
- `ClearKeyTableStack`
- `ClearSelection`
- `CloseCurrentPane`
- `CompleteSelection`
- `CompleteSelectionOrOpenLinkAtMouseCursor`
- `Confirmation`
- `CopyTextTo`
- `DetachDomain`
- `DisableDefaultAssignment`
- `EmitEvent`
- `ExtendSelectionToMouseCursor`
- `HideApplication`
- `InputSelector`
- `MoveTab`
- `Nop`
- `OpenLinkAtMouseCursor`
- `OpenUri`
- `PaneSelect`
- `PopKeyTable`
- `PromptInputLine`
- `QuickSelect`
- `QuickSelectArgs`
- `QuitApplication`
- `ResetFontAndWindowSize`
- `ResetTerminal`
- `RotatePanes`
- `ScrollByCurrentEventWheelDelta`
- `ScrollByLine`
- `ScrollToBottom`
- `ScrollToPrompt`
- `ScrollToTop`
- `SelectTextAtMouseCursor`
- `SendKey`
- `SendString`
- `SetPaneZoomState`
- `SetWindowLevel`
- `Show`
- `ShowLauncher`
- `ShowLauncherArgs`
- `ShowTabNavigator`
- `SpawnCommandInNewTab`
- `SpawnCommandInNewWindow`
- `SplitPane`
- `StartWindowDrag`
- `SwitchToWorkspace`
- `SwitchWorkspaceRelative`
- `ToggleAlwaysOnBottom`
- `ToggleAlwaysOnTop`

*Upstream comparison: wezterm 20260205_190134_4bf8878b*
