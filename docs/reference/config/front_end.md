---
tags:
  - gpu
---
# `front_end = "OpenGL"`

Specifies which render front-end to use.  This option used to have
more scope in earlier versions of wakterm, but today it allows three
possible values:

* `OpenGL` - use GPU accelerated rasterization
* `Software` - use CPU-based rasterization.
* `WebGpu` - use GPU accelerated rasterization

The default is `"OpenGL"`.

You may wish (or need!) to select `Software` if there are issues with your
GPU/OpenGL drivers.

wakterm will automatically select `Software` if it detects that it is
being started in a Remote Desktop environment on Windows.

## WebGpu

The WebGpu front end allows wakterm to use GPU acceleration provided by
a number of platform-specific backends:

* Metal (on macOS)
* Vulkan
* DirectX 12 (on Windows)

See also:

* [webgpu_preferred_adapter](webgpu_preferred_adapter.md)
* [webgpu_power_preference](webgpu_power_preference.md)
* [webgpu_force_fallback_adapter](webgpu_force_fallback_adapter.md)
