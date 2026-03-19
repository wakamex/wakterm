# `wakterm.serde.json_decode(string)`

{{since('nightly')}}

Parses the supplied string as `json` and returns the equivalent `lua` values:

```
> wakterm.serde.json_decode('{"foo":"bar"}')
{
    "foo": "bar",
}
```
