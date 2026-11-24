// Keys that switch a setting of one strip or bus: mute and solo. The key
// shows the setting as Lemon Mix has it, however it was changed.

import { action } from "@elgato/streamdeck";

import { find, type Snapshot, type Want } from "../model.js";
import type { PressSettings } from "../press.js";
import { title } from "./base.js";
import { OnOffAction, unavailable } from "./onoff.js";

type TargetSettings = { target?: string } & PressSettings;

abstract class SwitchAction extends OnOffAction<TargetSettings> {
	protected abstract readonly name: "mute" | "solo";
	protected abstract readonly want: Want;

	protected current(settings: TargetSettings, snapshot: Snapshot | undefined): boolean | undefined {
		const found = find(snapshot, settings.target, this.want);
		return found ? Boolean((found.view.params as unknown as Record<string, unknown>)[this.name]) : undefined;
	}

	protected describe(settings: TargetSettings, snapshot: Snapshot | undefined): string {
		return title(snapshot, find(snapshot, settings.target, this.want)?.view.label, Boolean(settings.target));
	}

	protected unavailable(settings: TargetSettings, snapshot: Snapshot | undefined): string {
		return unavailable(snapshot, Boolean(settings.target));
	}

	protected async set(settings: TargetSettings, on: boolean): Promise<unknown> {
		if (!settings.target) {
			throw new Error("No strip or bus is chosen.");
		}
		return this.mixer.control({ action: this.name, target: settings.target, on });
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
	protected readonly want = "either";
}
