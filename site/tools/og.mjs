// Renders tools/og.html to public/img/og.png. Needs playwright (npx playwright install chromium).
import { chromium } from "playwright";
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";
const here = dirname(fileURLToPath(import.meta.url));
const b = await chromium.launch();
const p = await b.newPage({ viewport: { width: 1200, height: 630 } });
await p.goto(pathToFileURL(join(here, "og.html")).href);
await p.screenshot({ path: join(here, "..", "public", "img", "og.png") });
await b.close();
