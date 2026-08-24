---
tags:
  - quick_select
---
# `quick_select_patterns`

Specify additional patterns to match when in [quick select mode](../../quickselect.md).
This setting is a table listing out a set of regular expressions.

```lua
config.quick_select_patterns = {
  -- match things that look like sha1 hashes
  -- (this is actually one of the default patterns)
  '[0-9a-f]{7,40}',
}
```

!!! note
    If you want to use capture groups in your patterns, you must use
    non-capturing groups `(?:)` for them to work as you intend, as
    the overall list of `quick_select_patterns` is compiled into a larger
    alternation regex that itself uses capture groups.

The regex syntax supports backreferences and look around assertions.
See [Fancy Regex Syntax](https://docs.rs/fancy-regex/latest/fancy_regex/#syntax)
for the extended syntax, which builds atop the underlying
[Regex syntax](https://docs.rs/regex/latest/regex/#syntax).

This example matches the string `"bar"`, but only when not part of the string
`"foo:bar"`:

```lua
config.quick_select_patterns = {
  '(?<!foo:)bar',
}
```
