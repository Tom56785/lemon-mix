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

## Press options

The Mute, Solo and Route keys share these settings:

| Setting | Description |
|---|---|
| Mode | **Toggle** changes the setting on each press. **Push** changes it while the key is held and puts it back on release: on a muted microphone that is push to talk, on one that is not, push to mute. |
| Toggle again on release | Toggle mode only. A press toggles straight away as usual; if you hold the key for at least the **Hold time**, releasing it toggles back. Off by default. |
| Hold time (ms) | How long the key must be held for its release to toggle back (1000 = 1 second). Defaults to 2000. |
| Show checkmark after press | Shows Stream Deck's checkmark after a press that worked. Off by default. |

If a press cannot be made (Lemon Mix is not running, say), the key shows Stream Deck's warning triangle. In a multi-action, a key sets the state you pick for that step, in either mode.

The keys have two states, off and on, and follow the mixer by switching between them. A title or icon you set for a state, with its position, font and colour, replaces the plugin's, as for any other action; without one, the key shows the strip's or bus's name.

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
| `npm test` | The tests: finding strips and buses, display values, how keys answer presses, and the connection to a stand-in for Lemon Mix. |
| `npm run typecheck` | Checks the TypeScript. |
| `npm run validate` | Checks the plugin with Elgato's validator. |
| `npm run pack` | Builds `dist/com.lemonmix.mixer.streamDeckPlugin`, which installs by double-clicking it. |
| `npm run icons` | Draws the images again after `scripts/make-icons.mjs` changes. |

## Layout

- `src/plugin.ts`: registers the actions and connects.
- `src/mixer.ts`: the connection to Lemon Mix. It keeps a subscription that reconnects on its own, and sends one-off requests.
- `src/model.ts`: the mixer's state and the rules for finding strips and buses. It has no Stream Deck code, so it is tested on its own.
- `src/press.ts`: how the on/off keys answer presses (Toggle, Push, toggling back after a long hold). It has no Stream Deck code either.
- `src/actions/`: the actions.
- `com.lemonmix.mixer.sdPlugin/`: the plugin as Stream Deck loads it: the manifest, the images, the dial layout and the settings pages (`ui/`).

The settings pages use [sdpi-components](https://sdpi-components.dev) 4.0.1 (MIT; it includes Lit, BSD-3-Clause), kept in `ui/sdpi-components.js` so the plugin loads nothing from the web.
