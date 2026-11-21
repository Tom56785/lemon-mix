// Draws the plugin's images into com.lemonmix.mixer.sdPlugin/imgs: the
// plugin icon (the application's lemon, as PNG, which Stream Deck requires
// for it) and the action and key images (SVG). Run after changing them:
//
//     npm run icons

import { mkdir, writeFile } from "node:fs/promises";
import { deflateSync } from "node:zlib";

const root = "com.lemonmix.mixer.sdPlugin/imgs";

// ---- The lemon, as in app/src/icon.rs ------------------------------------

function rotate(x, y, angle) {
	const [sin, cos] = [Math.sin(angle), Math.cos(angle)];
	return [x * cos - y * sin, x * sin + y * cos];
}

/** The colour at a point of the unit square, or nothing where it is transparent. */
function shade(x, y) {
	const [lx, ly] = rotate(x - 0.7, y - 0.2, 0.6);
	if ((lx / 0.17) ** 2 + (ly / 0.075) ** 2 <= 1) {
		const vein = Math.abs(ly) < 0.008 && Math.abs(lx) < 0.15;
		return vein ? [0.2, 0.45, 0.16] : [0.33, 0.62, 0.22];
	}
	const [ex, ey] = rotate(x - 0.48, y - 0.55, -0.52);
	const tip = (t) => Math.max(Math.abs(t) - 0.36, 0) * 1.6;
	const half = Math.max(0.3 - tip(ex), 1e-6); // half the height, narrowing to the tips
	const radius = (ex / 0.4) ** 2 + Math.min((ey / half) ** 2, 1e9);
	const nib = Math.abs(ex) <= 0.46 && Math.abs(ey) <= 0.035 && Math.abs(ex) > 0.38;
	if (radius > 1 && !nib) {
		return undefined;
	}
	const edge = radius > 0.86;
	const light = Math.min(Math.max(1 - Math.hypot(x - 0.36, y - 0.4) * 2.2, 0), 1);
	const base = edge ? [0.85, 0.62, 0.05] : [0.97, 0.8, 0.18];
	const highlight = [1, 0.95, 0.6];
	return base.map((b, i) => b + (highlight[i] - b) * light * 0.55);
}

function lemonRgba(size) {
	const sub = 4;
	const pixels = Buffer.alloc(size * size * 4);
	for (let y = 0; y < size; y++) {
		for (let x = 0; x < size; x++) {
			const sum = [0, 0, 0, 0];
			for (let sy = 0; sy < sub; sy++) {
				for (let sx = 0; sx < sub; sx++) {
					const c = shade((x + (sx + 0.5) / sub) / size, (y + (sy + 0.5) / sub) / size);
					if (c) {
						sum[0] += c[0];
						sum[1] += c[1];
						sum[2] += c[2];
						sum[3] += 1;
					}
				}
			}
			const i = (y * size + x) * 4;
			const alpha = sum[3] / (sub * sub);
			if (alpha > 0) {
				for (let c = 0; c < 3; c++) {
					pixels[i + c] = Math.round(Math.min(Math.max(sum[c] / sum[3], 0), 1) * 255);
				}
			}
			pixels[i + 3] = Math.round(alpha * 255);
		}
	}
	return pixels;
}

// ---- PNG ------------------------------------------------------------------

const CRC_TABLE = Array.from({ length: 256 }, (_, n) => {
	let c = n;
	for (let k = 0; k < 8; k++) {
		c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
	}
	return c >>> 0;
});

function crc32(bytes) {
	let c = 0xffffffff;
	for (const b of bytes) {
		c = CRC_TABLE[(c ^ b) & 0xff] ^ (c >>> 8);
	}
	return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
	const length = Buffer.alloc(4);
	length.writeUInt32BE(data.length);
	const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
	const crc = Buffer.alloc(4);
	crc.writeUInt32BE(crc32(body));
	return Buffer.concat([length, body, crc]);
}

function png(size, rgba) {
	const header = Buffer.alloc(13);
	header.writeUInt32BE(size, 0);
	header.writeUInt32BE(size, 4);
	header[8] = 8; // bit depth
	header[9] = 6; // RGBA
	const rows = Buffer.alloc(size * (size * 4 + 1));
	for (let y = 0; y < size; y++) {
		rows[y * (size * 4 + 1)] = 0; // no filter
		rgba.copy(rows, y * (size * 4 + 1) + 1, y * size * 4, (y + 1) * size * 4);
	}
	return Buffer.concat([
		Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
		chunk("IHDR", header),
		chunk("IDAT", deflateSync(rows, { level: 9 })),
		chunk("IEND", Buffer.alloc(0)),
	]);
}

// ---- SVG ------------------------------------------------------------------

const LEMON = "#F7D23E";
const RED = "#DC463C";
const AMBER = "#E6A01E";
const DIM = "#8A8A8A";
const KEY_BG = "#1C1C1C";

/** A key image, 144 x 144, with the glyph above where the title goes. */
const key = (glyph, background = KEY_BG) =>
	`<svg xmlns="http://www.w3.org/2000/svg" width="144" height="144" viewBox="0 0 144 144">` +
	`<rect width="144" height="144" fill="${background}"/>` +
	`<g transform="translate(36 14)">${glyph}</g></svg>\n`;

/** An action list icon, 20 x 20, white on transparent as Stream Deck asks. */
const icon = (glyph) =>
	`<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20" viewBox="0 0 72 72">${glyph}</svg>\n`;

// Glyphs on a 72 x 72 grid.
const speaker = (color) =>
	`<path d="M8 26h14l18-16v52L22 46H8z" fill="${color}"/>` +
	`<path d="M50 24c6 6 6 18 0 24M57 16c11 11 11 29 0 40" fill="none" stroke="${color}" stroke-width="5" stroke-linecap="round"/>`;
const mutedSpeaker = (color) =>
	`<path d="M8 26h14l18-16v52L22 46H8z" fill="${color}"/>` +
	`<path d="M48 26l18 20M66 26L48 46" stroke="${color}" stroke-width="6" stroke-linecap="round"/>`;
const letterS = (color) =>
	`<path d="M50 20c-4-5-10-7-16-7-9 0-15 5-15 12 0 16 34 9 34 26 0 7-7 12-17 12-7 0-13-3-17-8" fill="none" stroke="${color}" stroke-width="7" stroke-linecap="round"/>`;
const arrow = (color) =>
	`<circle cx="12" cy="36" r="8" fill="${color}"/>` +
	`<path d="M22 36h34M44 22l14 14-14 14" fill="none" stroke="${color}" stroke-width="7" stroke-linecap="round" stroke-linejoin="round"/>`;
const fader = (color) =>
	`<path d="M18 8v56M54 8v56" stroke="${color}" stroke-width="4" stroke-linecap="round" opacity="0.5"/>` +
	`<rect x="8" y="34" width="20" height="12" rx="3" fill="${color}"/><rect x="44" y="18" width="20" height="12" rx="3" fill="${color}"/>`;
const knob = (color) =>
	`<circle cx="36" cy="38" r="24" fill="none" stroke="${color}" stroke-width="6"/>` +
	`<path d="M36 38V20" stroke="${color}" stroke-width="6" stroke-linecap="round"/>`;
const lemonOutline = (color) =>
	`<ellipse cx="36" cy="40" rx="26" ry="19" transform="rotate(-30 36 40)" fill="none" stroke="${color}" stroke-width="5"/>` +
	`<path d="M44 16c6-8 14-9 20-8-2 7-8 12-18 12" fill="${color}"/>`;

const files = {
	"actions/mute.svg": icon(mutedSpeaker("#FFFFFF")),
	"actions/solo.svg": icon(letterS("#FFFFFF")),
	"actions/route.svg": icon(arrow("#FFFFFF")),
	"actions/volume.svg": icon(fader("#FFFFFF")),
	"actions/fader.svg": icon(knob("#FFFFFF")),
	"plugin/category.svg": icon(lemonOutline("#FFFFFF")),
	"keys/mute-off.svg": key(speaker("#E6E6E6")),
	"keys/mute-on.svg": key(mutedSpeaker("#FFFFFF"), RED),
	"keys/solo-off.svg": key(letterS(DIM)),
	"keys/solo-on.svg": key(letterS("#1C1C1C"), AMBER),
	"keys/route-off.svg": key(arrow(DIM)),
	"keys/route-on.svg": key(arrow("#1C1C1C"), LEMON),
	"keys/volume.svg": key(fader(LEMON)),
	"keys/fader.svg": key(knob(LEMON)),
};

for (const [path, svg] of Object.entries(files)) {
	await mkdir(`${root}/${path.split("/")[0]}`, { recursive: true });
	await writeFile(`${root}/${path}`, svg);
}
await mkdir(`${root}/plugin`, { recursive: true });
for (const [name, size] of [["icon.png", 256], ["icon@2x.png", 512]]) {
	await writeFile(`${root}/plugin/${name}`, png(size, lemonRgba(size)));
}
console.log(`Wrote ${Object.keys(files).length + 2} images to ${root}`);
