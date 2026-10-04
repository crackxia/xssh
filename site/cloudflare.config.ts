import { bindings, defineConfig } from "cf/config";
import * as entrypoint from "./src/index.ts" with { type: "cf-worker" };

export default defineConfig({
	worker: {
		name: "xssh-site",
		compatibilityDate: "2026-10-01",
		entrypoint,
		env: {
			// Release archives: releases/<version>/<file>.
			RELEASES: bindings.r2({ name: "xssh-releases" }),
			// Download counts (migrations/).
			DB: bindings.d1({ name: "xssh-site", id: "e2984564-fdd7-4575-a970-2aaf601eaf18" }),
			// The release manifest (key "manifest"), written by scripts/release.mjs.
			MANIFEST: bindings.kv({ id: "5266ed3c5078439c8d854f9d6e7d8d14" }),
			// Cloudflare Web Analytics site token (public; it is in every page).
			ANALYTICS_TOKEN: bindings.text("28ba4a02565841be8ff07cc25b94957f"),
		},
		domains: ["xssh.io", "www.xssh.io"],
		assets: { htmlHandling: "none", notFoundHandling: "none" },
		observability: { enabled: true, logs: { enabled: true, invocationLogs: true } },
	},
});
