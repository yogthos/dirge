# Custom themes

dirge ships two built-in palettes and supports user-defined themes
via JSON files in `~/.config/dirge/`.

## Built-ins

- `phosphor` (default) — 80s CRT green. Errors red, warnings yellow.
- `plain` — white assistant text, cyan accents, gray dim.

Switch via `theme` in `~/.config/dirge/config.json`:

```json
{
  "theme": "plain"
}
```

## Custom themes

Any value other than `phosphor` / `plain` is treated as the stem
of a theme file: dirge looks for
`~/.config/dirge/<name>.theme.json`. If found, its color fields
**override the phosphor preset**; anything not present in the
file stays phosphor. If the file is missing or malformed, dirge
falls back to phosphor with a warning rather than refusing to
start.

### Format

Every field is optional. A minimal one-color override:

```json
{
  "accent": "magenta"
}
```

A fully-custom palette:

```json
{
  "agent": "#88ccff",
  "user":  "#ffaa66",
  "system": "darkgray",
  "tool":   "darkcyan",
  "perm":   "yellow",
  "result": "darkgray",
  "critic":   "#c9a9ff",
  "thinking": "#8a9c90",
  "error":  "red",
  "warn":   "yellow",
  "accent": "#ff66cc",
  "dim":    "darkgray",
  "header": "cyan",
  "divider": "darkgray",
  "banner_primary":   "#aa88ff",
  "banner_secondary": "darkmagenta",
  "label":  "MIDNIGHT"
}
```

### Color values

Each color field accepts three forms:

1. **Named color** (case-insensitive, `_` and `-` separators
   both work):
   - `"black"`, `"red"`, `"green"`, `"yellow"`, `"blue"`,
     `"magenta"`, `"cyan"`, `"white"`, `"grey"` (or `"gray"`)
   - `"darkred"`, `"darkgreen"`, `"darkyellow"`, `"darkblue"`,
     `"darkmagenta"`, `"darkcyan"`, `"darkgrey"` (or `"darkgray"`)
   - `"reset"` (terminal default)

2. **Hex RGB**: `"#rrggbb"` (six hex digits). Renders as truecolor;
   terminals without truecolor support degrade gracefully.

3. **256-color palette index**: a plain integer 0–255 (no quotes).

### Field reference

| Field | What it colors |
|---|---|
| `agent` | Assistant chat text |
| `user` | User message prefix (`<you>`) |
| `system` | System messages — context loaded, compactions |
| `tool` | Tool chamber headers (`╭─ BASH ─ …`) |
| `perm` | Permission prompts (loud — yellow/red recommended) |
| `result` | Secondary result text (slash output, dim tool stdout) |
| `critic` | In-loop critic's review voice (`<critic>`) |
| `thinking` | Agent reasoning / "thinking" register (placeholder, Ctrl+O expand, Ctrl+R stream) |
| `error` | Hard errors (red recommended; keeps semantic urgency) |
| `warn` | Warnings (yellow recommended) |
| `accent` | Headers, focused picker rows, banner accent |
| `dim` | Placeholders, separators, low-noise hints |
| `header` | Right-panel headers |
| `divider` | Horizontal divider line |
| `banner_primary` | Welcome banner primary stroke |
| `banner_secondary` | Welcome banner border / decorations |
| `label` | Human-readable name shown in the banner |

### Activating

1. Create `~/.config/dirge/midnight.theme.json` with your override.
2. Set `theme: "midnight"` in `~/.config/dirge/config.json`.
3. Restart dirge.

### Errors

If something's wrong dirge prints a single warning on startup and
uses phosphor:

```
warning: theme 'midnight' could not be loaded (parse /home/y/.config/dirge/midnight.theme.json: trailing comma at line 4); using phosphor.
Custom themes live at ~/.config/dirge/<name>.theme.json.
```

The error message names the file path so it's clear what dirge
was trying to load.
