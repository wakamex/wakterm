---
tags:
  - gpu
---
# `webgpu_power_preference = "LowPower"`

Specifies the power preference when selecting a webgpu GPU instance.
This option is only applicable when you have configured `front_end = "WebGpu"`.

The possible values are:

* `"LowPower"` - use an integrated GPU
* `"HighPerformance"` - use a discrete GPU

You can have more fine grained control over which GPU is selected using
[webgpu_preferred_adapter](webgpu_preferred_adapter.md).
