# `wakterm.serde.toml_encode(value)`

Encodes the supplied `lua` value as `toml`:

```
> wakterm.serde.toml_encode({foo = { "bar", "baz", "qux" } })
"foo = [\"bar\", \"baz\", \"qux\"]\n"
```
