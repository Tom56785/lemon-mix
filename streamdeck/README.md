# Lemon Mix for Stream Deck

A Stream Deck plugin for Lemon Mix. Its keys and dials control strips and buses, and they show how the mixer is set however it was changed: from the window, the command line, or another key.

| Action | Device | What it does |
|---|---|---|
| **Mute** | Key | Mutes a strip or bus. The key turns red while it is muted. |
| **Solo** | Key | Solos a strip. The key lights while it is soloed. |
| **Route** | Key | Sends a strip to a bus, or stops it. The key lights while it sends. |
| **Volume** | Key | Moves a fader by a step (for example +3 or -3 dB), or sets it to a level. The title shows the level. |
| **Fader** | Dial (Stream Deck +) | Turn for volume, or hold the dial while turning for finer steps. Press or tap the screen to mute. The screen shows the level, the fader and a level meter. |

Each action's settings list the strips and buses while Lemon Mix runs. A key keeps following its strip or bus when others are added, removed or renamed, because it remembers the strip's key rather than its number or name. Keys show "Lemon Mix not running" until Lemon Mix starts, then pick it up on their own.

## Requirements

- Stream Deck 7.1 or later, on Windows 10 or later.
- Lemon Mix, running on demand or as the Windows service. The plugin talks to it over the local pipe described in [docs/control-protocol.md](../docs/control-protocol.md).

## Building and installing

You need Node.js 24 or later.

```
cd streamdeck
npm install
npm run link      # builds, then links the plugin into Stream Deck
npm run restart   # restarts the plugin after a rebuild
```

| Script | What it does |
|---|---|
| `npm run build` | Bundles `src/` into `com.lemonmix.mixer.sdPlugin/bin/plugin.js`. |
| `npm run watch` | The same, rebuilding on every change. |
| `npm test` | The tests: finding strips and buses, display values, and the connection to a stand-in for Lemon Mix. |
| `npm run typecheck` | Checks the TypeScript. |
| `npm run validate` | Checks the plugin with Elgato's validator. |
| `npm run pack` | Builds `dist/com.lemonmix.mixer.streamDeckPlugin`, which installs by double-clicking it. |
| `npm run icons` | Draws the images again after `scripts/make-icons.mjs` changes. |

## Layout

- `src/plugin.ts`: registers the actions and connects.
- `src/mixer.ts`: the connection to Lemon Mix. It keeps a subscription that reconnects on its own, and sends one-off requests.
- `src/model.ts`: the mixer's state and the rules for finding strips and buses. It has no Stream Deck code, so it is tested on its own.
- `src/actions/`: the actions.
- `com.lemonmix.mixer.sdPlugin/`: the plugin as Stream Deck loads it: the manifest, the images, the dial layout and the settings pages (`ui/`).

The settings pages use [sdpi-components](https://sdpi-components.dev) 4.0.1 (MIT; it includes Lit, BSD-3-Clause), kept in `ui/sdpi-components.js` so the plugin loads nothing from the web.
