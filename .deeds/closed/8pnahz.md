---
tier: objective
---
# Launcher script: place window on Hyprland workspace F1 and fullscreen

Agents running waygriff for testing must not hijack the user's desktop focus. The window should land on Hyprland workspace `F1` (on the HDMI-A-1 4K monitor) and go fullscreen there. Fullscreen also enables a more efficient presentation path in Hyprland.

Environment facts: this shell has no `HYPRLAND_INSTANCE_SIGNATURE`; the instance dir is `$XDG_RUNTIME_DIR/hypr/<sig>`. Hyprland is 0.56.2. Verify the windowrule syntax against that version rather than memory.

Deliverable: a script (or `waygriffctl` subcommand) that builds, launches headless with logs to a file, ensures placement, and reports the control socket is up.

Closed: waygriffctl start launches via hyprctl eval hl.exec_cmd with workspace/fullscreen rules; verified on F1 without focus change
