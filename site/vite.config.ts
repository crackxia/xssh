import { cloudflare } from "@cloudflare/vite-plugin";
import { defineConfig } from "vite";

export default defineConfig({
	plugins: [cloudflare()],
	// The docs page renders ../docs/guide.md (the agent manual) from the repository.
	server: { fs: { allow: [".."] } },
});
