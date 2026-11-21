// The mixer's state as Lemon Mix sends it, and what the plugin works out
// from it. Nothing here talks to Stream Deck, so it can be tested on its own.
// docs/control-protocol.md in the repository describes the messages.

/** The protocol version this plugin speaks. */
export const PROTOCOL_VERSION = 1;

/** The fader range, in dB. At the bottom the strip or bus is silent. */
export const MIN_GAIN_DB = -60;
export const MAX_GAIN_DB = 12;

export interface StripParams {
	gain_db: number;
	mute: boolean;
	solo: boolean;
	mono: boolean;
	pan: number;
}

export interface BusParams {
	gain_db: number;
	mute: boolean;
	mono: boolean;
	limiter: number | null;
}

export interface Meter {
	peak: [number, number];
	rms: [number, number];
	reduction: number;
}

export interface StripView {
	/** `H1`... for hardware strips, `V<cable id>` for virtual ones; changes when strips before it are removed. */
	id: string;
	/** Stays with the strip: what the plugin stores in its settings. */
	key: string;
	label: string;
	params: StripParams;
	/** The IDs of the buses it sends to. */
	routes: string[];
}

export interface BusView {
	id: string;
	key: string;
	label: string;
	params: BusParams;
}

export interface Snapshot {
	strips: StripView[];
	buses: BusView[];
}

export type Found = { kind: "strip"; view: StripView } | { kind: "bus"; view: BusView };

export type Want = "strip" | "bus" | "either";

/**
 * Finds what a target names, by the same rules as Lemon Mix: a key, an ID,
 * `strip:<name>` or `bus:<name>`, or a name only one strip or bus has.
 */
export function find(snapshot: Snapshot | undefined, target: string | undefined, want: Want = "either"): Found | undefined {
	if (!snapshot || !target) {
		return undefined;
	}
	const all: Found[] = [
		...(want === "bus" ? [] : snapshot.strips.map((view): Found => ({ kind: "strip", view }))),
		...(want === "strip" ? [] : snapshot.buses.map((view): Found => ({ kind: "bus", view }))),
	];
	const text = target.trim();
	const same = (a: string, b: string): boolean => a.localeCompare(b, undefined, { sensitivity: "accent" }) === 0;
	const exact = all.find((f) => f.view.key === text || same(f.view.id, text));
	if (exact) {
		return exact;
	}
	const prefix = /^(strip|bus):(.*)$/i.exec(text);
	const kind = prefix ? (prefix[1].toLowerCase() as "strip" | "bus") : undefined;
	const name = prefix ? prefix[2].trim() : text;
	const named = all.filter((f) => (!kind || f.kind === kind) && same(f.view.label, name));
	return named.length === 1 ? named[0] : undefined;
}

/** A fader position from 0 to 100, even in dB. */
export function gainPercent(db: number): number {
	return Math.round(((clamp(db, MIN_GAIN_DB, MAX_GAIN_DB) - MIN_GAIN_DB) / (MAX_GAIN_DB - MIN_GAIN_DB)) * 100);
}

/** A level meter position from 0 to 100: -60 dBFS to full scale. */
export function meterPercent(peak: number): number {
	if (!(peak > 0)) {
		return 0;
	}
	const dbfs = 20 * Math.log10(Math.min(peak, 1));
	return Math.round(clamp((dbfs + 60) / 60, 0, 1) * 100);
}

/** A gain as shown on a key or dial. */
export function formatGain(db: number): string {
	if (db <= MIN_GAIN_DB) {
		return "-inf dB";
	}
	const rounded = Math.round(db * 10) / 10;
	return `${rounded > 0 ? "+" : ""}${rounded.toFixed(1)} dB`;
}

/** A dB setting typed into a text field, or `fallback` when it is not a number. */
export function parseDb(value: unknown, fallback: number): number {
	const number = typeof value === "number" ? value : Number.parseFloat(String(value ?? "").replace(/db$/i, ""));
	return Number.isFinite(number) ? number : fallback;
}

export type Choice = {
	label: string;
	value: string;
};

export type ChoiceGroup = {
	label: string;
	children: Choice[];
};

/** The strips and buses a property inspector offers, by key, in groups. */
export function choices(snapshot: Snapshot | undefined, want: Want): ChoiceGroup[] {
	if (!snapshot) {
		return [];
	}
	const choice = (view: { id: string; key: string; label: string }): Choice => ({
		label: view.label === view.id ? view.id : `${view.label} (${view.id})`,
		value: view.key,
	});
	const groups: ChoiceGroup[] = [];
	if (want !== "bus" && snapshot.strips.length > 0) {
		groups.push({ label: "Inputs", children: snapshot.strips.map(choice) });
	}
	if (want !== "strip" && snapshot.buses.length > 0) {
		groups.push({ label: "Outputs", children: snapshot.buses.map(choice) });
	}
	return groups;
}

function clamp(value: number, min: number, max: number): number {
	return Math.min(max, Math.max(min, value));
}
