// Bundles the plugin into com.lemonmix.mixer.sdPlugin/bin/plugin.js, the
// file the manifest's CodePath names. `--watch` rebuilds on every change.

import { writeFile } from "node:fs/promises";

import * as esbuild from "esbuild";

const outdir = "com.lemonmix.mixer.sdPlugin/bin";
const options = {
	entryPoints: ["src/plugin.ts"],
	bundle: true,
	platform: "node",
	target: "node24",
	format: "esm",
	outfile: `${outdir}/plugin.js`,
	sourcemap: "linked",
	// Bundled CommonJS dependencies (ws) call require for Node's own modules.
	banner: { js: "import { createRequire } from 'node:module'; const require = createRequire(import.meta.url);" },
	logLevel: "info",
};

if (process.argv.includes("--watch")) {
	const context = await esbuild.context(options);
	await context.watch();
} else {
	await esbuild.build(options);
}
await writeFile(`${outdir}/package.json`, '{ "type": "module" }\n');
