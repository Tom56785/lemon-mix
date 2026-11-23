// How an on/off key (Mute, Solo, Route) answers presses: what to set when
// the key goes down, and what to set back when it comes up. It has no Stream
// Deck code, so it is tested on its own.

/** The settings an on/off key's press options are kept in. */
export type PressSettings = {
	/** `toggle` changes the setting on each press; `push` changes it while the key is held. */
	mode?: string;
	/** Toggle mode: releasing the key after holding it for `holdMs` changes the setting back. */
	toggleAgainWhenHeld?: boolean;
	/** How long the key must be held for its release to change the setting back, in ms. */
	holdMs?: string | number;
	/** Shows Stream Deck's checkmark after a press that worked. */
	showOk?: boolean;
};

/** How long a key must be held when its hold time is missing or not a number. */
export const DEFAULT_HOLD_MS = 2000;

/** The hold time in ms; a missing, invalid or zero time is the default. */
export function holdMs(value: PressSettings["holdMs"]): number {
	const ms = Number(value);
	return Number.isFinite(ms) && ms > 0 ? ms : DEFAULT_HOLD_MS;
}

/** A key held down whose release may set the setting back. */
type Held = { restore: boolean; at: number; push: boolean; holdMs: number };

export class Presses {
	/** Keys held down, by action ID. */
	readonly #held = new Map<string, Held>();

	/**
	 * The key went down with the setting at `current`: returns what to set it
	 * to. In a multi-action, the step sets the state chosen for it (`desired`):
	 * no key is held there, so a push could only undo itself.
	 */
	down(id: string, settings: PressSettings, current: boolean, now: number, desired?: number): boolean {
		this.#held.delete(id);
		if (desired !== undefined) {
			return desired === 1;
		}
		const push = settings.mode === "push";
		if (push || settings.toggleAgainWhenHeld) {
			this.#held.set(id, { restore: current, at: now, push, holdMs: holdMs(settings.holdMs) });
		}
		return !current;
	}

	/**
	 * The key came up: returns what to set the setting back to, or `undefined`
	 * to leave it. A push key puts back what it changed; a toggle key that
	 * toggles again does so only after a long enough hold.
	 */
	up(id: string, now: number): boolean | undefined {
		const held = this.#held.get(id);
		this.#held.delete(id);
		if (!held) {
			return undefined;
		}
		if (held.push || now - held.at >= held.holdMs) {
			return held.restore;
		}
		return undefined;
	}

	/** Forgets a key: its press failed, or it went away. */
	forget(id: string): void {
		this.#held.delete(id);
	}
}
