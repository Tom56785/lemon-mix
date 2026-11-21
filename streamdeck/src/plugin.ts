// The Lemon Mix Stream Deck plugin: keys and dials for its strips and buses,
// talking to Lemon Mix over its control pipe.

import streamDeck from "@elgato/streamdeck";

import { FaderAction } from "./actions/fader.js";
import { RouteAction } from "./actions/route.js";
import { MuteAction, SoloAction } from "./actions/switch.js";
import { VolumeAction } from "./actions/volume.js";
import { Mixer, PROTOCOL_MISMATCH_NOTE } from "./mixer.js";

const mixer = new Mixer();
let warned = false;
mixer.on("mismatch", (protocol) => {
	if (!warned) {
		warned = true;
		streamDeck.logger.warn(`Lemon Mix speaks protocol ${protocol}; ${PROTOCOL_MISMATCH_NOTE}`);
	}
});
mixer.on("state", (snapshot) => streamDeck.logger.info(snapshot ? "Connected to Lemon Mix" : "Lemon Mix is not running"));

streamDeck.actions.registerAction(new MuteAction(mixer));
streamDeck.actions.registerAction(new SoloAction(mixer));
streamDeck.actions.registerAction(new RouteAction(mixer));
streamDeck.actions.registerAction(new VolumeAction(mixer));
streamDeck.actions.registerAction(new FaderAction(mixer));

mixer.start();
await streamDeck.connect();
