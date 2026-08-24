---
title: wakterm.json_parse
tags:
 - utility
 - json
---

# `wakterm.json_parse(string)`

Parses the supplied string as json and returns the equivalent lua values:

```
> wakterm.json_parse('{"foo":"bar"}')
{
    "foo": "bar",
}
```
