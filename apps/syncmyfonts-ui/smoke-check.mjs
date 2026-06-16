import { chromium } from "playwright";
import { existsSync } from "node:fs";

const browserCandidates = [
  process.env.CHROME_PATH,
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
  "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
].filter(Boolean);
const chromePath = browserCandidates.find((candidate) => existsSync(candidate));
const appUrl = new URL("./dist/index.html", import.meta.url).href;
const outputDir = new URL("./smoke/", import.meta.url);
const views = ["sync", "fonts", "peers", "settings", "support"];
const platforms = ["macos", "windows"];
const viewports = [
  { name: "default", width: 1180, height: 760 },
  { name: "compact", width: 980, height: 700 },
  { name: "narrow", width: 760, height: 700 },
];

const browser = await chromium.launch(chromePath ? { executablePath: chromePath, headless: true } : { headless: true });

const results = {};

for (const viewport of viewports) {
  const page = await browser.newPage({
    viewport: { width: viewport.width, height: viewport.height },
    deviceScaleFactor: 1,
  });
  await page.goto(appUrl);
  results[viewport.name] = {};

  for (const platform of platforms) {
    await page.evaluate(async (platformName) => {
      if (typeof window.setPlatform === "function") {
        window.setPlatform(platformName);
      } else {
        document.body.dataset.platform = platformName;
      }
    }, platform);
    results[viewport.name][platform] = {};

    for (const view of views) {
      await page.click(`[data-view="${view}"]`);
      await page.mouse.move(4, viewport.height - 4);
      await page.screenshot({
        path: new URL(`ui-${viewport.name}-${platform}-${view}.png`, outputDir).pathname,
        fullPage: false,
      });
      results[viewport.name][platform][view] = await page.evaluate(() => {
        const overflowing = [];
        document.querySelectorAll("*").forEach((element) => {
          const rect = element.getBoundingClientRect();
          if (rect.right > window.innerWidth + 1 || rect.left < -1 || rect.top < -1) {
            overflowing.push({
              tag: element.tagName,
              className: String(element.className),
              text: element.textContent?.trim().replace(/\s+/g, " ").slice(0, 80),
              rect: {
                left: rect.left,
                top: rect.top,
                right: rect.right,
                bottom: rect.bottom,
              },
            });
          }
        });

        return {
          platform: document.body.dataset.platform,
          title: document.getElementById("viewTitle")?.textContent,
          activeNavCount: document.querySelectorAll(".nav-item.active").length,
          activePanelCount: document.querySelectorAll(".view-panel.active").length,
          activeView: document.querySelector(".nav-item.active")?.dataset.view,
          scrollWidth: document.documentElement.scrollWidth,
          clientWidth: document.documentElement.clientWidth,
          scrollHeight: document.documentElement.scrollHeight,
          clientHeight: document.documentElement.clientHeight,
          horizontalOverflow: document.documentElement.scrollWidth > document.documentElement.clientWidth + 1,
          overflowing,
        };
      });
    }
  }

  await page.close();
}

await browser.close();

const failures = [];
for (const [viewportName, viewportResult] of Object.entries(results)) {
  for (const [platformName, platformResult] of Object.entries(viewportResult)) {
    for (const [viewName, result] of Object.entries(platformResult)) {
      if (result.horizontalOverflow || result.overflowing.length > 0) {
        failures.push(`${viewportName}/${platformName}/${viewName}`);
      }
      if (result.activeNavCount !== 1 || result.activePanelCount !== 1 || result.activeView !== viewName) {
        failures.push(`${viewportName}/${platformName}/${viewName}:active-state`);
      }
    }
  }
}

console.log(JSON.stringify({ ok: failures.length === 0, failures, results }, null, 2));

if (failures.length > 0) {
  process.exitCode = 1;
}
