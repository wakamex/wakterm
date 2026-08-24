---
tags:
  - keys
---
# `use_ime`

Controls whether the Input Method Editor (IME) will be used to process keyboard
input.  The IME is useful for inputting kanji or other text that is not
natively supported by the attached keyboard hardware.

IME support is a platform dependent feature

|Platform|Notes|
|--------|-----|
|Windows|Always enabled and cannot be disabled|
|macOS|Enabled by default|
|X11|Uses [XIM](https://en.wikipedia.org/wiki/X_Input_Method). Your system needs a running input method engine such as ibus or fcitx that supports XIM.|
|Wayland|Your compositor must support `zwp_text_input_v3`|

You can control whether the IME is enabled in your configuration file:

```lua
config.use_ime = false
```

Changing `use_ime` usually requires re-launching wakterm to take full effect.
It defaults to `true`. On X11, ensure that `XMODIFIERS` or
[xim_im_name](xim_im_name.md) is configured before launching Wakterm. For
example, GNOME users will probably want `XMODIFIERS=@im=ibus`.
