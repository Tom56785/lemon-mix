// What the on/off keys (Mute, Solo, Route) share: the key's state shows the
// setting as Lemon Mix has it, however it was changed, and presses work in
// Toggle or Push mode, optionally toggling back after a long hold (see
// press.ts), with an optional checkmark.

import type { Action, KeyAction, KeyDownEvent, KeyUpEvent, WillAppearEvent, WillDisappearEvent } from "@elgato/streamdeck";

import type { Snapshot } from "../model.js";
import { type PressSettings, Presses } from "../press.js";
import { MixerAction, type Settings } from "./base.js";

type State = 0 | 1;

export abstract class OnOffAction<T extends Settings & PressSettings> extends MixerAction<T> {
	readonly #presses = new Presses();
	/** The state and title last sent to each key, so an unchanged key is not sent them again. */
	readonly #shown = new Map<string, { state: State; title: string }>();

	/** Whether the setting is on; `undefined` when it cannot be found (see `unavailable`). */
	protected abstract current(settings: T, snapshot: Snapshot | undefined): boolean | undefined;
	/** The key's title, unless the user has given it one. */
	protected abstract describe(settings: T, snapshot: Snapshot | undefined): string;
	/** Why the setting cannot be found, for the log. */
	protected abstract unavailable(settings: T, snapshot: Snapshot | undefined): string;
	/** Sets the setting; rejects with Lemon Mix's reason. */
	protected abstract set(settings: T, on: boolean): Promise<unknown>;

	protected async render(action: Action<T>, settings: T, snapshot: Snapshot | undefined): Promise<void> {
		// A multi-action step has no key face of its own.
		if (!action.isKey() || action.isInMultiAction()) {
			return;
		}
		await this.show(action, this.current(settings, snapshot) ? 1 : 0, this.describe(settings, snapshot));
	}

	private async show(action: KeyAction<T>, state: State, title: string): Promise<void> {
		const shown = this.#shown.get(action.id);
		this.#shown.set(action.id, { state, title });
		if (shown?.state !== state) {
			await action.setState(state);
		}
		if (shown?.title !== title) {
			await action.setTitle(title);
		}
	}

	override async onWillAppear(ev: WillAppearEvent<T>): Promise<void> {
		this.#shown.delete(ev.action.id);
		await super.onWillAppear(ev);
	}

	override onWillDisappear(ev: WillDisappearEvent<T>): void {
		super.onWillDisappear(ev);
		this.#shown.delete(ev.action.id);
		this.#presses.forget(ev.action.id);
	}

	override async onKeyDown(ev: KeyDownEvent<T>): Promise<void> {
		const { settings } = ev.payload;
		const current = this.current(settings, this.mixer.snapshot);
		const desired = ev.payload.isInMultiAction ? ev.payload.userDesiredState : undefined;
		const target = current === undefined ? undefined : this.#presses.down(ev.action.id, settings, current, Date.now(), desired);
		if (!(await this.apply(ev.action, settings, target))) {
			// Nothing changed, so there is nothing to put back on release.
			this.#presses.forget(ev.action.id);
		}
	}

	override async onKeyUp(ev: KeyUpEvent<T>): Promise<void> {
		const target = this.#presses.up(ev.action.id, Date.now());
		if (target !== undefined) {
			await this.apply(ev.action, ev.payload.settings, target);
		}
	}

	/** Sets the setting to `target`, showing it on the key at once; returns whether it worked. */
	private async apply(action: KeyAction<T>, settings: T, target: boolean | undefined): Promise<boolean> {
		const ok = await this.change(action, async () => {
			if (target === undefined) {
				throw new Error(this.unavailable(settings, this.mixer.snapshot));
			}
			await this.set(settings, target);
		});
		if (!ok || target === undefined) {
			return false;
		}
		if (!action.isInMultiAction()) {
			// The mixer's own word follows in a moment; this saves the wait.
			const title = this.#shown.get(action.id)?.title ?? this.describe(settings, this.mixer.snapshot);
			await this.show(action, target ? 1 : 0, title);
		}
		if (settings.showOk) {
			await action.showOk();
		}
		return true;
	}
}

/** Why a strip or bus cannot be found. */
export function unavailable(snapshot: Snapshot | undefined, chosen: boolean): string {
	if (!snapshot) {
		return "Lemon Mix is not running.";
	}
	return chosen ? "The strip or bus is not in Lemon Mix." : "No strip or bus is chosen.";
}
