# `wakterm.serde.yaml_encode(value)`

{{since('nightly')}}

Encodes the supplied `lua` value as `yaml`:

```
> wakterm.serde.yaml_encode({foo = "bar"})
"foo: bar\n"
```
