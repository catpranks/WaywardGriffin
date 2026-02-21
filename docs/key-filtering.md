# Key filtering and remapping

## Config

`$XDG_CONFIG_HOME/waygriff/config.toml` (typically `~/.config/waygriff/config.toml`).

```toml
[keys]
block = ["KEY_F14", "KEY_F15", "KEY_F16", "KEY_F17", "KEY_F18", "KEY_F19", "KEY_F20", "KEY_F21", "KEY_F22", "KEY_F23", "KEY_F24"]

[keys.remap]
KEY_F13 = "KEY_F"
```

Key names are evdev `KeyCode` names, parsed via `evdev::KeyCode::from_str`. Case-sensitive, exact match.

## Data structures

New module `src/config.rs`:

```rust
struct KeyConfig {
    block: HashSet<u16>,       // evdev keycodes to drop
    remap: HashMap<u16, u16>,  // evdev keycode -> evdev keycode
}
```

TOML is deserialized with string keys/values, then resolved to `u16` codes at load time. Parse errors are fatal (bad key name = typo the user should fix).

`KeyConfig` gets a method like:

```rust
fn transform(&self, raw_code: u32) -> Option<u32>
```

Returns `None` to block, `Some(remapped)` to forward (identity if not in remap table).

## Wiring

1. `lib.rs:run()` loads config before spawning threads.
2. `KeyConfig` passed into `InputState` (lives on display thread, no sharing needed).
3. `display/input.rs` `press_key`/`release_key`: call `transform(event.raw_code)` before forwarding to bridge. `None` = swallow the event.

## Dependencies

- `evdev` — `KeyCode` + `FromStr`
- `toml` — TOML parsing
- `serde`, `serde_derive` — deserialization
- `xdg` — XDG base directory lookup

## Steps

1. `cargo add evdev toml serde xdg` (serde with `derive` feature)
2. Create `src/config.rs`: raw TOML struct (serde), `KeyConfig` with resolution and `transform()`
3. Load config in `lib.rs:run()`, pass into display thread → `InputState`
4. Apply `transform()` in `press_key`/`release_key` in `display/input.rs`
5. Missing config file = no filtering (empty block set, empty remap table)
