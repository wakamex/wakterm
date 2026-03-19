---
title: wakterm.json_parse
tags:
 - utility
 - json
---


# `wakterm.json_parse(string)`

{{since('20220807-113146-c2fee766')}}

Parses the supplied string as json and returns the equivalent lua values:

```
> wakterm.json_parse('{"foo":"bar"}')
{
    "foo": "bar",
}
```
