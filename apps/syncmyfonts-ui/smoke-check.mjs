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
const workflowSteps = ["share", "pair", "preview", "install"];

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
      await page.locator(`[data-view="${view}"]`).evaluate((element) => element.blur());
      if (view === "sync") {
        await page.click('[data-step="share"]');
        await page.locator('[data-step="share"]').evaluate((element) => element.blur());
      }
      await page.mouse.move(viewport.width - 8, viewport.height - 8);
      await page.waitForTimeout(180);
      await page.screenshot({
        path: new URL(`ui-${viewport.name}-${platform}-${view}.png`, outputDir).pathname,
        fullPage: false,
      });
      results[viewport.name][platform][view] = await page.evaluate(() => {
        const overflowing = [];
        const body = document.body;
        const appShell = document.querySelector(".app-shell");
        const activeSyncFlow = document.querySelector(".view-panel.active .sync-flow");
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
          activeStepCount: activeSyncFlow?.querySelectorAll(".flow-step.active").length ?? 0,
          activeStep: activeSyncFlow?.querySelector(".flow-step.active")?.dataset.step ?? "",
          flowProgress: activeSyncFlow ? getComputedStyle(activeSyncFlow).getPropertyValue("--flow-progress").trim() : "",
          scrollWidth: document.documentElement.scrollWidth,
          clientWidth: document.documentElement.clientWidth,
          bodyScrollWidth: body.scrollWidth,
          bodyClientWidth: body.clientWidth,
          appShellScrollWidth: appShell?.scrollWidth ?? 0,
          appShellClientWidth: appShell?.clientWidth ?? 0,
          scrollHeight: document.documentElement.scrollHeight,
          clientHeight: document.documentElement.clientHeight,
          horizontalOverflow:
            document.documentElement.scrollWidth > document.documentElement.clientWidth + 1 ||
            body.scrollWidth > body.clientWidth + 1 ||
            (appShell ? appShell.scrollWidth > appShell.clientWidth + 1 : false),
          overflowing,
        };
      });

      if (view === "sync") {
        results[viewport.name][platform][view].workflowSteps = {};
        for (const step of workflowSteps) {
          await page.click(`[data-step="${step}"]`);
          results[viewport.name][platform][view].workflowSteps[step] = await page.evaluate(() => {
            const syncFlow = document.querySelector(".sync-flow");
            return {
              activeStepCount: document.querySelectorAll(".flow-step.active").length,
              activeStep: document.querySelector(".flow-step.active")?.dataset.step,
              flowProgress: syncFlow ? getComputedStyle(syncFlow).getPropertyValue("--flow-progress").trim() : "",
              stageTitle: document.getElementById("flowStageTitle")?.textContent,
              detailTitle: document.getElementById("stepTitle")?.textContent,
            };
          });
        }
      }
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
      if (viewName === "sync") {
        for (const [stepName, stepResult] of Object.entries(result.workflowSteps ?? {})) {
          if (stepResult.activeStepCount !== 1 || stepResult.activeStep !== stepName) {
            failures.push(`${viewportName}/${platformName}/${viewName}:${stepName}:workflow-state`);
          }
          if (!stepResult.flowProgress || stepResult.stageTitle !== stepResult.detailTitle) {
            failures.push(`${viewportName}/${platformName}/${viewName}:${stepName}:workflow-copy`);
          }
        }
      }
    }
  }
}

console.log(JSON.stringify({ ok: failures.length === 0, failures, results }, null, 2));

if (failures.length > 0) {
  process.exitCode = 1;
}
