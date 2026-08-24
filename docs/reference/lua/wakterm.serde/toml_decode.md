# `wakterm.serde.toml_decode(string)`

Parses the supplied string as `toml` and returns the equivalent `lua` values:

```
> wakterm.serde.toml_decode('foo = "bar"')
{
    "foo": "bar",
}
```
