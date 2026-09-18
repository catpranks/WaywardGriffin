# Key filtering and remapping

Optional config at `$XDG_CONFIG_HOME/waygriff/config.toml` (usually `~/.config/waygriff/config.toml`). Missing file means no filtering.

```toml
[keys]
block = ["KEY_F14", "KEY_F15", "KEY_F16", "KEY_F17", "KEY_F18", "KEY_F19", "KEY_F20", "KEY_F21", "KEY_F22", "KEY_F23", "KEY_F24"]

[keys.remap]
KEY_F13 = "KEY_F"
```

Key names are evdev `KeyCode` names, case-sensitive. An unknown name is a fatal error at startup.

Blocked keys are swallowed before forwarding; remapped keys are forwarded as the target code. Applied in `display/input.rs` on every key press and release via `Config::transform_key`; parsing lives in `src/config.rs`.
