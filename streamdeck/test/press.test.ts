import assert from "node:assert/strict";
import { test } from "node:test";

import { DEFAULT_HOLD_MS, holdMs, Presses } from "../src/press.js";

test("toggle keys change the setting on each press, and nothing on release", () => {
	const presses = new Presses();
	assert.equal(presses.down("k", {}, false, 0), true);
	assert.equal(presses.up("k", 5000), undefined, "even after a long hold");
	assert.equal(presses.down("k", { mode: "toggle" }, true, 0), false);
	assert.equal(presses.up("k", 10), undefined);
});

test("push keys change the setting while held and put it back on release", () => {
	const presses = new Presses();
	// Unmuted: muted while held (push to mute).
	assert.equal(presses.down("k", { mode: "push" }, false, 0), true);
	assert.equal(presses.up("k", 10), false);
	// Muted: unmuted while held (push to talk).
	assert.equal(presses.down("k", { mode: "push" }, true, 0), false);
	assert.equal(presses.up("k", 10_000), true);
	assert.equal(presses.up("k", 10_001), undefined, "one release per press");
});

test("toggle keys can toggle again on release after a long hold", () => {
	const presses = new Presses();
	const settings = { toggleAgainWhenHeld: true, holdMs: "1500" };
	assert.equal(presses.down("k", settings, false, 1000), true);
	assert.equal(presses.up("k", 2499), undefined, "a short press only toggles");
	assert.equal(presses.down("k", settings, true, 3000), false);
	assert.equal(presses.up("k", 4500), true, "held for the hold time: back again");
	// The default hold time when none is set.
	assert.equal(presses.down("k", { toggleAgainWhenHeld: true }, false, 0), true);
	assert.equal(presses.up("k", DEFAULT_HOLD_MS - 1), undefined);
	assert.equal(presses.down("k", { toggleAgainWhenHeld: true }, false, 0), true);
	assert.equal(presses.up("k", DEFAULT_HOLD_MS), false);
});

test("keys are kept apart, and a failed press is forgotten", () => {
	const presses = new Presses();
	presses.down("a", { mode: "push" }, false, 0);
	presses.down("b", { mode: "push" }, true, 0);
	assert.equal(presses.up("b", 1), true);
	assert.equal(presses.up("a", 1), false);
	presses.down("a", { mode: "push" }, false, 0);
	presses.forget("a");
	assert.equal(presses.up("a", 1), undefined);
});

test("in a multi-action, a step sets the state chosen for it", () => {
	const presses = new Presses();
	assert.equal(presses.down("k", { mode: "push" }, true, 0, 1), true);
	assert.equal(presses.up("k", 10), undefined, "nothing to put back");
	assert.equal(presses.down("k", {}, true, 0, 0), false);
	assert.equal(presses.down("k", { toggleAgainWhenHeld: true }, false, 0, 0), false);
	assert.equal(presses.up("k", 99_999), undefined);
});

test("hold times that are missing or not numbers are the default", () => {
	assert.equal(holdMs("750"), 750);
	assert.equal(holdMs(250), 250);
	for (const bad of [undefined, "", "0", "-5", "soon"]) {
		assert.equal(holdMs(bad), DEFAULT_HOLD_MS, String(bad));
	}
});
