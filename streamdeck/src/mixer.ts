// The connection to Lemon Mix over its control pipe: a subscription that
// keeps the state (and, while a dial needs them, the meters) up to date and
// comes back on its own when Lemon Mix starts again, and one-off requests.

import { EventEmitter } from "node:events";
import net from "node:net";

import { PROTOCOL_VERSION, type Snapshot } from "./model.js";

export const CONTROL_PIPE = "\\\\.\\pipe\\LemonMixControl";
/** How long to wait before connecting again. */
const RETRY_MS = 2000;
/** How long a request may take. */
const REQUEST_TIMEOUT_MS = 3000;
/** Logged when Lemon Mix is newer than this plugin. */
export const PROTOCOL_MISMATCH_NOTE = "this plugin speaks an older one, so some keys may not work: update the plugin.";

export interface KeyedMeter {
	key: string;
	peak: [number, number];
	rms: [number, number];
	reduction: number;
}

export interface MeterEvent {
	strips: KeyedMeter[];
	buses: KeyedMeter[];
}

export interface Reply {
	error: string | null;
	protocol?: number;
	snapshot: Snapshot;
}

/** A change, as `Request::Control` takes it. */
export type ControlAction =
	| { action: "mute" | "solo" | "mono"; target: string; on?: boolean | null }
	| { action: "route"; strip: string; bus: string; on?: boolean | null }
	| { action: "gain" | "adjust_gain"; target: string; db: number };

interface MixerEvents {
	/** The state changed, or a connection was made (`snapshot`) or lost (`undefined`). */
	state: [snapshot: Snapshot | undefined];
	meters: [meters: MeterEvent];
	/** A newer Lemon Mix speaks a protocol this plugin does not know. */
	mismatch: [protocol: number];
}

/** Splits a stream into lines. */
export class LineReader {
	#buffer = "";

	push(chunk: string): string[] {
		this.#buffer += chunk;
		const lines = this.#buffer.split("\n");
		this.#buffer = lines.pop() ?? "";
		return lines.filter((line) => line.length > 0);
	}
}

export class Mixer extends EventEmitter<MixerEvents> {
	#socket: net.Socket | undefined;
	#retry: NodeJS.Timeout | undefined;
	#meters = false;
	#stopped = true;
	#snapshot: Snapshot | undefined;
	/** The connection being closed to change the meter choice, which keeps the state. */
	#switching: net.Socket | undefined;

	constructor(private readonly pipe: string = CONTROL_PIPE) {
		super();
	}

	/** The latest state, while connected. */
	get snapshot(): Snapshot | undefined {
		return this.#snapshot;
	}

	get connected(): boolean {
		return this.#snapshot !== undefined;
	}

	/** Starts the subscription; it carries on until `stop`. */
	start(): void {
		this.#stopped = false;
		this.#connect();
	}

	stop(): void {
		this.#stopped = true;
		clearTimeout(this.#retry);
		this.#socket?.destroy();
		this.#socket = undefined;
	}

	/** Asks for meters, or stops them; the subscription starts again to say so. */
	setMeters(on: boolean): void {
		if (on === this.#meters) {
			return;
		}
		this.#meters = on;
		if (this.#socket) {
			// Ending the connection makes the next one, with the new choice. The
			// state stays meanwhile: Lemon Mix has not gone, so keys do not flicker.
			this.#switching = this.#socket;
			this.#socket.destroy();
		}
	}

	#connect(): void {
		if (this.#stopped || this.#socket) {
			return;
		}
		const socket = net.connect(this.pipe);
		this.#socket = socket;
		const lines = new LineReader();
		let opened = false;
		socket.setEncoding("utf8");
		socket.on("connect", () => {
			opened = true;
			socket.write(`${JSON.stringify({ op: "subscribe", meters: this.#meters })}\n`);
		});
		socket.on("data", (chunk: string) => {
			for (const line of lines.push(chunk)) {
				this.#receive(line);
			}
		});
		socket.on("error", () => {
			// "close" follows, which connects again.
		});
		socket.on("close", () => {
			if (this.#socket === socket) {
				this.#socket = undefined;
			}
			const switching = this.#switching === socket;
			if (switching) {
				this.#switching = undefined;
			} else if (this.#snapshot) {
				this.#snapshot = undefined;
				this.emit("state", undefined);
			}
			if (!this.#stopped) {
				clearTimeout(this.#retry);
				// A connection that was open (a meter choice change, or Lemon Mix
				// stopping) is tried again at once; one that failed, after a pause.
				this.#retry = setTimeout(() => this.#connect(), opened ? 50 : RETRY_MS);
			}
		});
	}

	#receive(line: string): void {
		let event: { event?: string; protocol?: number; snapshot?: Snapshot } & Partial<MeterEvent>;
		try {
			event = JSON.parse(line);
		} catch {
			return;
		}
		if (event.event === "state" && event.snapshot) {
			if (event.protocol !== undefined && event.protocol > PROTOCOL_VERSION) {
				this.emit("mismatch", event.protocol);
			}
			this.#snapshot = event.snapshot;
			this.emit("state", event.snapshot);
		} else if (event.event === "meters" && event.strips && event.buses) {
			this.emit("meters", { strips: event.strips, buses: event.buses });
		}
	}

	/** Sends a change; resolves with the state after it, rejects with Lemon Mix's reason. */
	control(action: ControlAction): Promise<Reply> {
		return this.request({ op: "control", action });
	}

	/** Sends a request on a connection of its own and waits for the reply. */
	request(request: object): Promise<Reply> {
		return new Promise((resolve, reject) => {
			const socket = net.connect(this.pipe);
			const lines = new LineReader();
			const timer = setTimeout(() => {
				socket.destroy();
				reject(new Error("Lemon Mix did not answer."));
			}, REQUEST_TIMEOUT_MS);
			socket.setEncoding("utf8");
			socket.on("connect", () => socket.write(`${JSON.stringify(request)}\n`));
			socket.on("data", (chunk: string) => {
				const [line] = lines.push(chunk);
				if (line === undefined) {
					return;
				}
				clearTimeout(timer);
				socket.end();
				try {
					const reply = JSON.parse(line) as Reply;
					if (reply.error) {
						reject(new Error(reply.error));
					} else {
						resolve(reply);
					}
				} catch {
					reject(new Error("Lemon Mix sent an invalid reply."));
				}
			});
			socket.on("error", (err: NodeJS.ErrnoException) => {
				clearTimeout(timer);
				reject(new Error(err.code === "ENOENT" ? "Lemon Mix is not running." : err.message));
			});
		});
	}
}
