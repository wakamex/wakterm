---
title: wakterm.json_encode
tags:
 - utility
 - json
---

# `wakterm.json_encode(value)`

{{since('20220807-113146-c2fee766')}}

Encodes the supplied lua value as json:

```
> wakterm.json_encode({foo = "bar"})
"{\"foo\":\"bar\"}"
```
