import assert from "node:assert/strict";
import { test } from "node:test";

import { choices, find, formatGain, gainPercent, meterPercent, parseDb, type Snapshot } from "../src/model.js";

const strip = (id: string, key: string, label: string) => ({
	id,
	key,
	label,
	params: { gain_db: 0, mute: false, solo: false, mono: false, pan: 0 },
	routes: ["A1"],
});
const bus = (id: string, key: string, label: string) => ({
	id,
	key,
	label,
	params: { gain_db: 0, mute: false, mono: false, limiter: null },
});

const snapshot: Snapshot = {
	strips: [strip("H1", "k-h1", "Mic"), strip("V1", "k-v1", "Game")],
	buses: [bus("A1", "k-a1", "A1"), bus("B1", "k-b1", "Game")],
};

test("targets are found as Lemon Mix finds them", () => {
	assert.equal(find(snapshot, "k-h1")?.view.id, "H1", "by key");
	assert.equal(find(snapshot, "h1")?.view.id, "H1", "by ID, ignoring case");
	assert.equal(find(snapshot, "MIC")?.view.id, "H1", "by name");
	assert.equal(find(snapshot, "Game"), undefined, "a strip and a bus share the name");
	assert.equal(find(snapshot, "bus:game")?.view.id, "B1");
	assert.equal(find(snapshot, "strip: Game")?.view.id, "V1");
	assert.equal(find(snapshot, "Game", "strip")?.view.id, "V1", "only strips are wanted");
	assert.equal(find(snapshot, "A1", "strip"), undefined);
	assert.equal(find(snapshot, undefined), undefined);
	assert.equal(find(undefined, "Mic"), undefined);
});

test("faders, meters and levels are shown in range", () => {
	assert.deepEqual([gainPercent(-60), gainPercent(0), gainPercent(12), gainPercent(99)], [0, 83, 100, 100]);
	assert.deepEqual([meterPercent(0), meterPercent(1), meterPercent(0.001), meterPercent(2)], [0, 100, 0, 100]);
	assert.equal(meterPercent(Number.NaN), 0);
	assert.deepEqual([formatGain(-6), formatGain(3.04), formatGain(0), formatGain(-60)], ["-6.0 dB", "+3.0 dB", "0.0 dB", "-inf dB"]);
	assert.deepEqual([parseDb("-3", 1), parseDb("2.5dB", 1), parseDb("", 1), parseDb(undefined, 1), parseDb(4, 1)], [-3, 2.5, 1, 1, 4]);
});

test("property inspectors list strips and buses by key", () => {
	assert.deepEqual(choices(snapshot, "bus"), [
		{
			label: "Outputs",
			children: [
				{ label: "A1", value: "k-a1" },
				{ label: "Game (B1)", value: "k-b1" },
			],
		},
	]);
	assert.deepEqual(
		choices(snapshot, "either").map((g) => g.label),
		["Inputs", "Outputs"],
	);
	assert.deepEqual(choices(undefined, "either"), []);
});
