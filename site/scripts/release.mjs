// Publish a release to xssh.io: upload the archives to R2 and update the manifest in KV.
//
//   node scripts/release.mjs <version> [windows-x86_64=<zip> ...]
//
// Notes and date come from releases/<version>.json ({ "date": "YYYY-MM-DD", "notes": { "en": [], "zh": [] } }).
// Without file arguments, ../target/package/xssh-<version>-windows-x86_64.zip is used.
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { readFileSync, statSync, writeFileSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, basename } from "node:path";

const BUCKET = "xssh-releases";
const KV = "5266ed3c5078439c8d854f9d6e7d8d14";

const cf = (...args) => execFileSync("cf", args, { encoding: "utf8", shell: process.platform === "win32" });

const [version, ...pairs] = process.argv.slice(2);
if (!/^\d+\.\d+\.\d+$/.test(version ?? "")) {
	console.error("usage: node scripts/release.mjs <version> [target=<zip> ...]");
	process.exit(64);
}
const meta = JSON.parse(readFileSync(`releases/${version}.json`, "utf8"));
const inputs = pairs.length ? pairs : [`windows-x86_64=../target/package/xssh-${version}-windows-x86_64.zip`];

const files = inputs.map((p) => {
	const [target, path] = p.split("=");
	const buf = readFileSync(path);
	const name = basename(path);
	const sha256 = createHash("sha256").update(buf).digest("hex");
	console.log(`upload ${name} (${buf.length} bytes, sha256 ${sha256})`);
	cf("r2", "objects", "put", `releases/${version}/${name}`, "--bucket-name", BUCKET, "--file", path, "--content-type", "application/zip");
	return { target, name, size: statSync(path).size, sha256 };
});

let manifest = { latest: version, releases: [] };
try {
	const cur = JSON.parse(cf("kv", "keys", "get", "manifest", "--namespace-id", KV, "--text"));
	if (cur?.releases) manifest = cur;
} catch {
	console.log("no manifest in KV yet; starting a new one");
}
manifest.releases = manifest.releases.filter((r) => r.version !== version);
manifest.releases.push({ version, date: meta.date, notes: meta.notes, files });
const key = (v) => v.split(".").map(Number);
manifest.releases.sort((a, b) => {
	const [x, y] = [key(a.version), key(b.version)];
	return y[0] - x[0] || y[1] - x[1] || y[2] - x[2];
});
manifest.latest = manifest.releases[0].version;

const out = join(mkdtempSync(join(tmpdir(), "xssh-release-")), "manifest.json");
writeFileSync(out, JSON.stringify(manifest));
cf("kv", "keys", "put", "manifest", "--namespace-id", KV, "--file", out);
console.log(`manifest: latest ${manifest.latest}, ${manifest.releases.length} release(s)`);
