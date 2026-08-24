---
tags:
  - clipboard
---
# `canonicalize_pasted_newlines`

Controls whether pasted text will have newlines normalized.

If bracketed paste mode is enabled by the application, the effective
value of this configuration option is `"None"`.

The following values are accepted:

|value|meaning|
|-----|-------|
|`true` |same as `"CarriageReturnAndLineFeed"`|
|`false` |same as `"None"`|
|`"None"` |The text is passed through unchanged|
|`"LineFeed"` |Newlines of any style are rewritten as LF|
|`"CarriageReturn"` |Newlines of any style are rewritten as CR|
|`"CarriageReturnAndLineFeed"` |Newlines of any style are rewritten as CRLF|

The default is `"CarriageReturnAndLineFeed"` on Windows and
`"CarriageReturn"` on other platforms.

On Windows we're in a bit of a frustrating situation: pasting into
Windows console programs requires CRLF otherwise there is no newline
at all, but when in WSL, pasting with CRLF gives excess blank lines.

In practice, the default setting means that unix shells and vim will get the
unix newlines in their pastes (which is the UX most users will want) and
cmd.exe will get CRLF.
