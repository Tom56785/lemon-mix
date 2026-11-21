// What every Lemon Mix action shares: drawing itself whenever the mixer's
// state changes, offering the strips and buses to its property inspector,
// and showing an alert when a change is refused.

import streamDeck, {
	type Action,
	type DidReceiveSettingsEvent,
	SingletonAction,
	type SendToPluginEvent,
	type WillAppearEvent,
	type WillDisappearEvent,
} from "@elgato/streamdeck";
import type { JsonValue } from "@elgato/utils";

import type { Mixer } from "../mixer.js";
import { choices, type Snapshot, type Want } from "../model.js";

export type Settings = { [key: string]: string | number | boolean | null | undefined };

/** The data sources property inspectors ask for, and what each lists. */
const DATA_SOURCES: Record<string, Want> = { getTargets: "either", getStrips: "strip", getBuses: "bus" };

export abstract class MixerAction<T extends Settings> extends SingletonAction<T> {
	/** The settings of each visible action, by its ID. */
	protected readonly settings = new Map<string, T>();

	constructor(protected readonly mixer: Mixer) {
		super();
		mixer.on("state", (snapshot) => {
			void this.renderAll();
			void this.updateInspector(snapshot);
		});
	}

	/** Draws one action from the mixer's state (`undefined` while Lemon Mix is not running). */
	protected abstract render(action: Action<T>, settings: T, snapshot: Snapshot | undefined): Promise<void>;

	protected async renderAll(): Promise<void> {
		for (const action of this.actions) {
			const settings = this.settings.get(action.id);
			if (settings) {
				await this.render(action, settings, this.mixer.snapshot);
			}
		}
	}

	override async onWillAppear(ev: WillAppearEvent<T>): Promise<void> {
		this.settings.set(ev.action.id, ev.payload.settings);
		await this.render(ev.action, ev.payload.settings, this.mixer.snapshot);
	}

	override onWillDisappear(ev: WillDisappearEvent<T>): void {
		this.settings.delete(ev.action.id);
	}

	override async onDidReceiveSettings(ev: DidReceiveSettingsEvent<T>): Promise<void> {
		this.settings.set(ev.action.id, ev.payload.settings);
		await this.render(ev.action, ev.payload.settings, this.mixer.snapshot);
	}

	override async onSendToPlugin(ev: SendToPluginEvent<JsonValue, T>): Promise<void> {
		const event = (ev.payload as { event?: string } | null)?.event;
		const want = event ? DATA_SOURCES[event] : undefined;
		if (event && want) {
			await streamDeck.ui.sendToPropertyInspector({ event, items: choices(this.mixer.snapshot, want) });
		}
	}

	/** Keeps an open property inspector's lists in line with the mixer. */
	private async updateInspector(snapshot: Snapshot | undefined): Promise<void> {
		if (streamDeck.ui.action?.manifestId !== this.manifestId) {
			return;
		}
		for (const [event, want] of Object.entries(DATA_SOURCES)) {
			await streamDeck.ui.sendToPropertyInspector({ event, items: choices(snapshot, want) });
		}
	}

	/** Makes a change, with an alert on the key or dial when it is refused. */
	protected async change(action: Action<T>, make: () => Promise<unknown>): Promise<void> {
		try {
			await make();
		} catch (err) {
			streamDeck.logger.warn(`${this.manifestId}: ${err instanceof Error ? err.message : String(err)}`);
			if (action.isKey() || action.isDial()) {
				await action.showAlert();
			}
		}
	}
}

/** A key's title: the strip's or bus's name, or why there is none. */
export function title(snapshot: Snapshot | undefined, label: string | undefined, chosen: boolean): string {
	if (!snapshot) {
		return "Lemon Mix\nnot running";
	}
	if (!chosen) {
		return "Choose in\nsettings";
	}
	return label ?? "Not found";
}
