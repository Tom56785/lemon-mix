// A key that sends a strip to a bus, lit while it does.

import { type Action, action, type KeyDownEvent } from "@elgato/streamdeck";

import { find, type Snapshot } from "../model.js";
import { MixerAction } from "./base.js";

type RouteSettings = { strip?: string; bus?: string };

@action({ UUID: "com.lemonmix.mixer.route" })
export class RouteAction extends MixerAction<RouteSettings> {
	protected async render(action: Action<RouteSettings>, settings: RouteSettings, snapshot: Snapshot | undefined): Promise<void> {
		if (!action.isKey()) {
			return;
		}
		const strip = find(snapshot, settings.strip, "strip");
		const bus = find(snapshot, settings.bus, "bus");
		const on = strip?.kind === "strip" && bus !== undefined && strip.view.routes.includes(bus.view.id);
		await action.setState(on ? 1 : 0);
		let text: string;
		if (!snapshot) {
			text = "Lemon Mix\nnot running";
		} else if (!settings.strip || !settings.bus) {
			text = "Choose in\nsettings";
		} else {
			text = `${strip?.view.label ?? "?"}\n→ ${bus?.view.label ?? "?"}`;
		}
		await action.setTitle(text);
	}

	override async onKeyDown(ev: KeyDownEvent<RouteSettings>): Promise<void> {
		const { strip, bus } = ev.payload.settings;
		await this.change(ev.action, async () => {
			if (!strip || !bus) {
				throw new Error("No strip or bus is chosen.");
			}
			await this.mixer.control({ action: "route", strip, bus });
		});
	}
}
