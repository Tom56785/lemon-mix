// A dial (Stream Deck +) that is a strip's or bus's fader: turning it moves
// the fader, pressing it or tapping the screen mutes, and the screen shows
// the level, the fader and a meter.

import {
	type Action,
	action,
	type DialDownEvent,
	type DialRotateEvent,
	type TouchTapEvent,
	type WillAppearEvent,
	type WillDisappearEvent,
} from "@elgato/streamdeck";

import type { MeterEvent, Mixer } from "../mixer.js";
import { find, formatGain, gainPercent, meterPercent, parseDb, type Snapshot } from "../model.js";
import { MixerAction } from "./base.js";

type FaderSettings = { target?: string; step?: string | number };

/** dB per dial tick for a new dial. */
const DEFAULT_STEP_DB = 1;
const LEMON = "#F7D23E";
const MUTED = "#6E6E6E";

@action({ UUID: "com.lemonmix.mixer.fader" })
export class FaderAction extends MixerAction<FaderSettings> {
	constructor(mixer: Mixer) {
		super(mixer);
		mixer.on("meters", (meters) => void this.showMeters(meters));
	}

	protected async render(action: Action<FaderSettings>, settings: FaderSettings, snapshot: Snapshot | undefined): Promise<void> {
		if (!action.isDial()) {
			return;
		}
		const found = find(snapshot, settings.target);
		if (!found) {
			await action.setFeedback({
				title: snapshot ? (settings.target ? "Not found" : "Choose in settings") : "Lemon Mix",
				value: snapshot ? "" : "not running",
				gain: { value: 0, bar_fill_c: MUTED },
				meter: { value: 0 },
			});
			return;
		}
		const { gain_db, mute } = found.view.params;
		await action.setFeedback({
			title: found.view.label,
			value: mute ? "Muted" : formatGain(gain_db),
			gain: { value: gainPercent(gain_db), bar_fill_c: mute ? MUTED : LEMON },
		});
	}

	override async onWillAppear(ev: WillAppearEvent<FaderSettings>): Promise<void> {
		await super.onWillAppear(ev);
		this.mixer.setMeters(true);
		if (ev.action.isDial()) {
			await ev.action.setTriggerDescription({ rotate: "Volume", push: "Mute", touch: "Mute" });
		}
	}

	override onWillDisappear(ev: WillDisappearEvent<FaderSettings>): void {
		super.onWillDisappear(ev);
		// Meters cost a little for every message: only while a dial shows them.
		this.mixer.setMeters(this.settings.size > 0);
	}

	override async onDialRotate(ev: DialRotateEvent<FaderSettings>): Promise<void> {
		const { target, step } = ev.payload.settings;
		await this.change(ev.action, async () => {
			if (!target) {
				throw new Error("No strip or bus is chosen.");
			}
			// Pressed while turning moves in finer steps.
			const db = ev.payload.ticks * parseDb(step, DEFAULT_STEP_DB) * (ev.payload.pressed ? 0.25 : 1);
			await this.mixer.control({ action: "adjust_gain", target, db });
		});
	}

	override async onDialDown(ev: DialDownEvent<FaderSettings>): Promise<void> {
		await this.toggleMute(ev.action, ev.payload.settings);
	}

	override async onTouchTap(ev: TouchTapEvent<FaderSettings>): Promise<void> {
		await this.toggleMute(ev.action, ev.payload.settings);
	}

	private async toggleMute(action: Action<FaderSettings>, settings: FaderSettings): Promise<void> {
		await this.change(action, async () => {
			if (!settings.target) {
				throw new Error("No strip or bus is chosen.");
			}
			await this.mixer.control({ action: "mute", target: settings.target });
		});
	}

	private async showMeters(meters: MeterEvent): Promise<void> {
		const snapshot = this.mixer.snapshot;
		for (const action of this.actions) {
			const found = find(snapshot, this.settings.get(action.id)?.target);
			if (!found || !action.isDial()) {
				continue;
			}
			const list = found.kind === "strip" ? meters.strips : meters.buses;
			const peak = list.find((m) => m.key === found.view.key)?.peak ?? [0, 0];
			await action.setFeedback({ meter: { value: meterPercent(Math.max(peak[0], peak[1])) } });
		}
	}
}
