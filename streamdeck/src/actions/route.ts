// A key that sends a strip to a bus, lit while it does.

import { action } from "@elgato/streamdeck";

import { find, type Snapshot } from "../model.js";
import type { PressSettings } from "../press.js";
import { OnOffAction, unavailable } from "./onoff.js";

type RouteSettings = { strip?: string; bus?: string } & PressSettings;

@action({ UUID: "com.lemonmix.mixer.route" })
export class RouteAction extends OnOffAction<RouteSettings> {
	protected current(settings: RouteSettings, snapshot: Snapshot | undefined): boolean | undefined {
		const strip = find(snapshot, settings.strip, "strip");
		const bus = find(snapshot, settings.bus, "bus");
		if (strip?.kind !== "strip" || !bus) {
			return undefined;
		}
		return strip.view.routes.includes(bus.view.id);
	}

	protected describe(settings: RouteSettings, snapshot: Snapshot | undefined): string {
		if (!snapshot) {
			return "Lemon Mix\nnot running";
		}
		if (!settings.strip || !settings.bus) {
			return "Choose in\nsettings";
		}
		const strip = find(snapshot, settings.strip, "strip");
		const bus = find(snapshot, settings.bus, "bus");
		return `${strip?.view.label ?? "?"}\n→ ${bus?.view.label ?? "?"}`;
	}

	protected unavailable(settings: RouteSettings, snapshot: Snapshot | undefined): string {
		return unavailable(snapshot, Boolean(settings.strip && settings.bus));
	}

	protected async set(settings: RouteSettings, on: boolean): Promise<unknown> {
		if (!settings.strip || !settings.bus) {
			throw new Error("No strip or bus is chosen.");
		}
		return this.mixer.control({ action: "route", strip: settings.strip, bus: settings.bus, on });
	}
}
