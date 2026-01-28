# tips
- on-startup = [{ type = "exec", exec = { shell = "setfacl -m u:otheruser:rw $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" } }]

# tasks
- [x] save command_buffer in capture force_render
  - ifl.last_command_buffer = Some(command_buffer.clone()); (except for typeshit)