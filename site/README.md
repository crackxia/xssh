# xssh.io

Cloudflare Worker (SSR) + static assets. Deployed with the `cf` CLI.

- `npm i` · `npx vite --host 127.0.0.1` (dev) · `npx cf deploy`
- Bindings (`cloudflare.config.ts`): R2 `RELEASES` (zips), D1 `DB` (download counts, `migrations/`), KV `MANIFEST` (release list), Web Analytics token.
- Release: build the zip, write `releases/<version>.json` (date, notes en/zh), then `node scripts/release.mjs <version>` (uploads to R2, updates KV).
- Pages: `/`, `/docs` (renders `../docs/guide.md`), `/download`; Chinese under `/zh/`. Agent entry: `/llms.txt`, `/llms-full.txt`, `/guide.md`, `/install.ps1`, `/latest.json`.
- Design: `../DESIGN.md`. Bump `ASSET_V` in `src/html.ts` after CSS/JS changes.
