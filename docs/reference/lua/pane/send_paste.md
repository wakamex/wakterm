# `pane:send_paste(text)`

Sends the supplied `text` string to the input of the pane as if it
were pasted from the clipboard, except that the clipboard is not involved.
Newlines are rewritten according to the
[`canonicalize_pasted_newlines`](../../config/canonicalize_pasted_newlines.md) setting.

If the terminal attached to the pane is set to bracketed paste mode then
the text will be sent as a bracketed paste, and newlines will not be rewritten.
