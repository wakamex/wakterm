# wakterm Hotkeys Reference

Auto-generated from source. Run `python3 generate-hotkeys.py > HOTKEYS.md` to update.
  
Upstream: `upstream/main` (05343b387)

## Default Key Bindings

| Action | Description | Linux/Win | macOS | Upstream |
|--------|-------------|-----------|-------|----------|
| `ActivateCommandPalette` | Activate Command Palette | Ctrl+Shift+p | Ctrl+Shift+p | same |
| `ActivateCopyMode` | Activate Copy Mode | Ctrl+Shift+x | Ctrl+Shift+x | same |
| `ActivatePaneDirection(Down)` | Activate Pane Down | Ctrl+Shift+DownArrow | Ctrl+Shift+DownArrow | same |
| `ActivatePaneDirection(Right)` | Activate Pane Right | Ctrl+Shift+RightArrow | Ctrl+Shift+RightArrow | same |
| `ActivatePaneDirection(Up)` | Activate Pane Up | Ctrl+Shift+UpArrow | Ctrl+Shift+UpArrow | same |
| `ActivateTab(-1)` | Activate right-most tab | Ctrl+Shift+9 | Cmd+9 | same |
| `ActivateTabRelativeNoWrap(n)` | Reload configuration | Ctrl+Shift+r | Cmd+r | same |
| `ActivateWindowRelativeNoWrap(n)` | Activate the tab to the left | Ctrl+Shift+[, Ctrl+Shift+Tab, Ctrl+PageUp | Shift+Cmd+[, Ctrl+Shift+Tab, Ctrl+PageUp | same |
| `AdjustPaneSize(Next` | Activate Pane Left | Ctrl+Shift+LeftArrow | Ctrl+Shift+LeftArrow | same |
| `CharSelect(_)` | Enter Emoji / Character selection mode | Ctrl+Shift+u | Ctrl+Shift+u | same |
| `ClearScrollback(ScrollbackOnly)` | Clear scrollback | Ctrl+Shift+k | Cmd+k | same |
| `CloseCurrentTab(confirm=true)` | Close current Tab | Ctrl+Shift+w | Cmd+w | same |
| `CopyTextTo(destination=Clipboard)` | Copy to clipboard | Ctrl+Shift+c, Copy | Cmd+c, Copy | same |
| `CopyTextTo(destination=ClipboardAndPrimarySelec...` | Copy to clipboard and primary selection | Ctrl+Insert | Ctrl+Insert | same |
| `CopyTextTo(destination=PrimarySelection)` | Copy to primary selection | Ctrl+Insert | Ctrl+Insert | same |
| `DecreaseFontSize` | Decrease font size | Ctrl+Shift+-, Ctrl+- | Cmd+-, Ctrl+- | same |
| `Hide` | Hide/Minimize Window | Ctrl+Shift+m | Cmd+m | same |
| `HideApplication` | Hide Application | Ctrl+Shift+h | Cmd+h | same |
| `IncreaseFontSize` | Increase font size | Ctrl+Shift+=, Ctrl+= | Cmd+=, Ctrl+= | same |
| `MoveTabRelative(-1)` | Move tab one place to the left | Ctrl+Shift+PageUp | Ctrl+Shift+PageUp | same |
| `MoveTabRelative(1)` | Move tab one place to the right | Ctrl+Shift+PageDown | Ctrl+Shift+PageDown | same |
| `PasteFrom(Clipboard)` | Paste from clipboard | Ctrl+Shift+v, Paste | Cmd+v, Paste | same |
| `PasteFrom(PrimarySelection)` | Paste primary selection | Shift+Insert | Shift+Insert | same |
| `QuickSelect` | Enter QuickSelect mode | Ctrl+Shift+Space | Ctrl+Shift+Space | same |
| `QuitApplication` | Quit wakterm | Ctrl+Shift+q | Cmd+q | same |
| `ResetFontSize` | Reset font size | Ctrl+Shift+0, Ctrl+0 | Cmd+0, Ctrl+0 | same |
| `Search(CurrentSelectionOrEmptyString)` | Search pane output | Ctrl+Shift+f | Cmd+f | same |
| `ShowDebugOverlay` | Show debug overlay | Ctrl+Shift+l | Ctrl+Shift+l | same |
| `SpawnTab(CurrentPaneDomain)` | New Tab | Ctrl+Shift+t | Cmd+t | same |
| `SpawnWindow` | New Window | Ctrl+Shift+n | Cmd+n | same |
| `ToggleFullScreen` | Toggle full screen mode | Alt+Return | Opt+Return | same |
| `TogglePaneZoomState` | Toggle Pane Zoom | Ctrl+Shift+z | Ctrl+Shift+z | same |

## Actions Without Default Bindings

| Action | Description | Upstream |
|--------|-------------|----------|
| `"https://github.com/wakamex/wakterm/discussions/"` | Discuss on GitHub | same |
| `"https://github.com/wakamex/wakterm/issues/"` | Search or report issue on GitHub | same |
| `ActivateLastTab` | Activate the last active tab | same |
| `ActivateTab(n)` | Activate Tab(n) | same |
| `ActivateTabRelative(1)` | Activate the tab to the right | same |
| `ActivateTabRelative(n)` | Activate the tab to the left (no wrapping) | same |
| `ActivateTabRelativeNoWrap(1)` | Activate the tab to the right (no wrapping) | same |
| `ActivateWindow(n)` | Activate the preceeding window | same |
| `ActivateWindowRelative(1)` | Activate the next window | same |
| `ActivateWindowRelative(n)` | Activate the preceeding window | same |
| `ActivateWindowRelativeNoWrap(1)` | Activate the next window | same |
| `AdjustPaneSize(Down, amount)` | Adjust Pane Size(Down, amount) | same |
| `AdjustPaneSize(Left, amount)` | Adjust Pane Size(Left, amount) | same |
| `AdjustPaneSize(Right, amount)` | Adjust Pane Size(Right, amount) | same |
| `AdjustPaneSize(Up, amount)` | Adjust Pane Size(Up, amount) | same |
| `AttachDomain(name)` | Attach Domain(name) | same |
| `ClearKeyTableStack` | Clear the key table stack | same |
| `ClearScrollback(ScrollbackAndViewport)` | Clear the scrollback and viewport | same |
| `ClearSelection` | Clears the selection in the current pane | same |
| `CloseCurrentPane(confirm=false)` | Close current Pane | same |
| `CloseCurrentPane(confirm=true)` | Close current Pane | same |
| `CloseCurrentTab(confirm=false)` | Close current Tab | same |
| `CompleteSelection(destination)` | Complete Selection(destination) | same |
| `CompleteSelectionOrOpenLinkAtMouseCursor(destination)` | Complete Selection Or Open Link At Mouse Cursor(destination) | same |
| `Confirmation(_)` | Prompt the user for confirmation | same |
| `CopyMode(copy_mode)` | Copy Mode(copy_mode) | same |
| `DetachDomain(CurrentPaneDomain)` | Detach the domain of the active pane | same |
| `DetachDomain(DefaultDomain)` | Detach the default domain | same |
| `DetachDomain(DomainId(id))` | Detach Domain(Domain Id(id)) | same |
| `DetachDomain(DomainName(name))` | Detach Domain(Domain Name(name)) | same |
| `DisableDefaultAssignment` | Disable Default Assignment | same |
| `EmitEvent(name)` | Emit Event(name) | same |
| `ExtendSelectionToMouseCursor(mode)` | Extend Selection To Mouse Cursor(mode) | same |
| `InputSelector(_)` | Prompt the user to choose from a list | same |
| `MoveTabRelative(n)` | Scrolls based on the mouse wheel position  in the current mouse event | same |
| `Multiple(actions)` | Multiple(actions) | same |
| `Nop` | Does nothing | same |
| `OpenLinkAtMouseCursor` | Open link at mouse cursor | same |
| `OpenUri(uri)` | Documentation | same |
| `PaneSelect(PaneSelectArguments(mode=Activate, ..))` | Enter Pane selection mode | same |
| `PaneSelect(PaneSelectArguments(mode=MoveToNewTab, ..))` | Move a pane into its own tab | same |
| `PaneSelect(PaneSelectArguments(mode=MoveToNewWindow, ..))` | Move a pane into its own window | same |
| `PaneSelect(PaneSelectArguments(mode=SwapWithActive, ..))` | Swap a pane with the active pane | same |
| `PaneSelect(PaneSelectArguments(mode=SwapWithActiveKeepFocus, ..))` | Swap a pane with the active pane, keeping focus | same |
| `PopKeyTable` | Pop the current key table | same |
| `PromptInputLine(_)` | Prompt the user for a line of text | same |
| `QuickSelectArgs(_)` | Enter QuickSelect mode | same |
| `ResetFontAndWindowSize` | Reset the window and font size | same |
| `RotatePanes(direction)` | Rotate Panes(direction) | same |
| `ScrollToBottom` | Scroll to the bottom | same |
| `ScrollToTop` | Scroll to the top | same |
| `Search(_)` | Search pane output | same |
| `SendKey(key)` | Send Key(key) | same |
| `SendString(text)` | Send String(text) | same |
| `SetPaneZoomState(false)` | Set Pane Zoom State(false) | same |
| `SetWindowLevel(AlwaysOnBottom)` | Always on Bottom | same |
| `SetWindowLevel(AlwaysOnTop)` | Always on Top | same |
| `SetWindowLevel(Normal)` | Normal | same |
| `Show` | Show/Restore Window | same |
| `ShowLauncherArgs(_)` | Show the launcher | same |
| `ShowTabNavigator` | Navigate tabs | same |
| `SpawnCommandInNewTab(cmd)` | Spawn Command In New Tab(cmd) | same |
| `SpawnCommandInNewWindow(cmd)` | Spawn Command In New Window(cmd) | same |
| `SpawnTab(DefaultDomain)` | New Tab (Default Domain) | same |
| `SpawnTab(DomainId(id))` | Spawn Tab(Domain Id(id)) | same |
| `SpawnTab(DomainName(name))` | Spawn Tab(Domain Name(name)) | same |
| `SplitHorizontal` | Split Horizontal | same |
| `SplitHorizontal(_)` | Split Horizontal(_) | same |
| `SplitPane(split)` | Reset the terminal emulation state in the current pane | same |
| `SplitVertical` | Split Vertical | same |
| `SplitVertical(_)` | Split Vertical(_) | same |
| `StartWindowDrag` | Requests a window drag operation from  the window environment | same |
| `SwitchToWorkspace(name=None, spawn=Some(prog))` | Switch To Workspace(name=None, spawn=Some(prog)) | same |
| `SwitchToWorkspace(name=Some(name), spawn=None)` | Switch To Workspace(name=Some(name), spawn=None) | same |
| `SwitchToWorkspace(name=Some(name), spawn=Some(prog))` | Switch To Workspace(name=Some(name), spawn=Some(prog)) | same |
| `SwitchWorkspaceRelative(n)` | Switch Workspace Relative(n) | same |
| `ToggleAlwaysOnBottom` | Toggle always on Bottom | same |
| `ToggleAlwaysOnTop` | Toggle always on Top | same |
| `_` | _ | same |

## Raw Actions (no command palette entry)

- `ActivateKeyTable`
- `ActivatePaneByIndex`
- `CopyTo`
- `MoveTab`
- `ReloadConfiguration`
- `ResetTerminal`
- `ScrollByCurrentEventWheelDelta`
- `ScrollByLine`
- `ScrollByPage`
- `ScrollToPrompt`
- `SelectTextAtMouseCursor`
- `ShowLauncher`

*32 bound, 79 unbound with description, 12 raw.*
