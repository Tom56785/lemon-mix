// The client against a stand-in for Lemon Mix's control pipe.

import assert from "node:assert/strict";
import { once } from "node:events";
import net from "node:net";
import { test } from "node:test";

import { LineReader, Mixer } from "../src/mixer.js";
import type { Snapshot } from "../src/model.js";

const empty: Snapshot = { strips: [], buses: [] };

/** A pipe server that answers like Lemon Mix, recording what it was sent. */
function fakeMixer(name: string) {
	const received: object[] = [];
	const subscribers: net.Socket[] = [];
	const server = net.createServer((socket) => {
		const lines = new LineReader();
		socket.setEncoding("utf8");
		socket.on("data", (chunk: string) => {
			for (const line of lines.push(chunk)) {
				const request = JSON.parse(line);
				received.push(request);
				if (request.op === "subscribe") {
					subscribers.push(socket);
					socket.write(`${JSON.stringify({ event: "state", protocol: 1, version: "test", snapshot: empty })}\n`);
					if (request.meters) {
						socket.write(`${JSON.stringify({ event: "meters", strips: [], buses: [] })}\n`);
					}
				} else {
					const error = request.action?.target === "Nobody" ? "There is no strip or bus named \"Nobody\"." : null;
					socket.end(`${JSON.stringify({ snapshot: empty, version: "test", protocol: 1, error })}\n`);
				}
			}
		});
		socket.on("error", () => {});
	});
	server.listen(name);
	return { server, received, subscribers };
}

test("lines are split however the data arrives", () => {
	const lines = new LineReader();
	assert.deepEqual(lines.push('{"a":1}\n{"b"'), ['{"a":1}']);
	assert.deepEqual(lines.push(":2}\n\n"), ['{"b":2}']);
});

test("the mixer subscribes, sends changes and follows Lemon Mix coming and going", async () => {
	const name = `\\\\.\\pipe\\LemonMixPluginTest${process.pid}`;
	const mixer = new Mixer(name);
	const states: (Snapshot | undefined)[] = [];
	mixer.on("state", (s) => states.push(s));

	// Not running: requests say so, and the subscription keeps trying.
	await assert.rejects(mixer.control({ action: "mute", target: "Mic" }), /not running/);
	mixer.start();

	const fake = fakeMixer(name);
	await once(mixer, "state");
	assert.equal(mixer.connected, true);
	assert.deepEqual(fake.received[0], { op: "subscribe", meters: false });

	await mixer.control({ action: "mute", target: "Mic" });
	await assert.rejects(mixer.control({ action: "solo", target: "Nobody" }), /no strip or bus named/);
	assert.deepEqual(fake.received.slice(1), [
		{ op: "control", action: { action: "mute", target: "Mic" } },
		{ op: "control", action: { action: "solo", target: "Nobody" } },
	]);

	// Asking for meters subscribes again with them, keeping the state
	// meanwhile: keys do not flicker to "not running".
	const statesBefore = states.length;
	const meters = once(mixer, "meters");
	mixer.setMeters(true);
	await meters;
	assert.deepEqual(fake.received.at(-1), { op: "subscribe", meters: true });
	assert.ok(states.slice(statesBefore).every((s) => s !== undefined), "no state was lost while switching");
	assert.equal(mixer.connected, true);

	// Lemon Mix stops: the state goes, and comes back when it does.
	fake.server.close();
	for (const socket of fake.subscribers) {
		socket.destroy();
	}
	while (states.at(-1) !== undefined) {
		await once(mixer, "state");
	}
	assert.equal(mixer.connected, false);
	const again = fakeMixer(name);
	while (states.at(-1) === undefined) {
		await once(mixer, "state");
	}
	mixer.stop();
	again.server.close();
	for (const socket of again.subscribers) {
		socket.destroy();
	}
});
