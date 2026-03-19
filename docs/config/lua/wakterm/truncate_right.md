---
title: wakterm.truncate_right
tags:
 - utility
 - string
---
# wakterm.truncate_right(string, max_width)

{{since('20210502-130208-bff6815d')}}

Returns a copy of `string` that is no longer than `max_width` columns
(as measured by [wakterm.column_width](column_width.md)).

Truncation occurs by reemoving excess characters from the right end
of the string.

For example, `wakterm.truncate_right("hello", 3)` returns `"hel"`,

See also: [wakterm.truncate_left](truncate_left.md), [wakterm.pad_left](pad_left.md).
