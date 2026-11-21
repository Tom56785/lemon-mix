// A key that moves a fader by a step (volume up or down), or sets it to a
// level, showing the level it is at.

import { type Action, action, type KeyDownEvent } from "@elgato/streamdeck";

import { find, formatGain, parseDb, type Snapshot } from "../model.js";
import { MixerAction, title } from "./base.js";

type VolumeSettings = { target?: string; mode?: string; db?: string | number };

/** The step a new key moves by. */
const DEFAULT_STEP_DB = 3;

@action({ UUID: "com.lemonmix.mixer.volume" })
export class VolumeAction extends MixerAction<VolumeSettings> {
	protected async render(action: Action<VolumeSettings>, settings: VolumeSettings, snapshot: Snapshot | undefined): Promise<void> {
		if (!action.isKey()) {
			return;
		}
		const found = find(snapshot, settings.target);
		const name = title(snapshot, found?.view.label, Boolean(settings.target));
		await action.setTitle(found ? `${name}\n${found.view.params.mute ? "muted" : formatGain(found.view.params.gain_db)}` : name);
	}

	override async onKeyDown(ev: KeyDownEvent<VolumeSettings>): Promise<void> {
		const { target, mode, db } = ev.payload.settings;
		await this.change(ev.action, async () => {
			if (!target) {
				throw new Error("No strip or bus is chosen.");
			}
			if (mode === "set") {
				await this.mixer.control({ action: "gain", target, db: parseDb(db, 0) });
			} else {
				await this.mixer.control({ action: "adjust_gain", target, db: parseDb(db, DEFAULT_STEP_DB) });
			}
		});
	}
}
