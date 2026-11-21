// Keys that switch a setting of one strip or bus: mute and solo. The key
// shows the setting as Lemon Mix has it, however it was changed.

import { type Action, action, type KeyDownEvent } from "@elgato/streamdeck";

import type { Mixer } from "../mixer.js";
import { find, type Snapshot, type Want } from "../model.js";
import { MixerAction, title } from "./base.js";

type TargetSettings = { target?: string };

abstract class SwitchAction extends MixerAction<TargetSettings> {
	protected abstract readonly name: "mute" | "solo";
	protected abstract readonly want: Want;

	constructor(mixer: Mixer) {
		super(mixer);
	}

	protected async render(action: Action<TargetSettings>, settings: TargetSettings, snapshot: Snapshot | undefined): Promise<void> {
		if (!action.isKey()) {
			return;
		}
		const found = find(snapshot, settings.target, this.want);
		const on = found ? Boolean((found.view.params as unknown as Record<string, unknown>)[this.name]) : false;
		await action.setState(on ? 1 : 0);
		await action.setTitle(title(snapshot, found?.view.label, Boolean(settings.target)));
	}

	override async onKeyDown(ev: KeyDownEvent<TargetSettings>): Promise<void> {
		const target = ev.payload.settings.target;
		await this.change(ev.action, async () => {
			if (!target) {
				throw new Error("No strip or bus is chosen.");
			}
			await this.mixer.control({ action: this.name, target });
		});
	}
}

@action({ UUID: "com.lemonmix.mixer.mute" })
export class MuteAction extends SwitchAction {
	protected readonly name = "mute";
	protected readonly want = "either";
}

@action({ UUID: "com.lemonmix.mixer.solo" })
export class SoloAction extends SwitchAction {
	protected readonly name = "solo";
	protected readonly want = "strip";
}
