// Replay turns user evidence into an executable reproduction. The same
// function is the reproducer (against HEAD) and the verifier (against the
// candidate): it performs the user's recorded actions and reports what it
// observed. It never decides "fixed" — the pipeline does.
const { chromium } = require("playwright");

async function replayIncident(incident, baseURL, screenshotPath) {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const responses = [];
  const pageErrors = [];
  page.on("response", (r) => {
    try {
      const u = new URL(r.url());
      responses.push({ method: r.request().method(), path: u.pathname, status: r.status() });
    } catch {}
  });
  page.on("pageerror", (e) => pageErrors.push(String(e.message).slice(0, 300)));

  const failure = incident.failure || {};
  const steps = [];
  try {
    await page.goto(baseURL + (incident.url || "/"), { waitUntil: "load", timeout: 15000 });
    steps.push(`open ${incident.url || "/"}`);
    const traceSteps = (incident.trace || []).filter(
      // For a friction incident the repeated dead clicks are replayed as a
      // measured probe below, not as ordinary steps.
      (s) => !(failure.kind === "friction" && s.kind === "click" && s.selector === failure.selector)
    );
    for (const step of traceSteps) {
      if (step.kind === "fill" && step.selector) {
        const value = step.value === "•••" ? "redacted" : step.value;
        await page.fill(step.selector, value, { timeout: 8000 });
        steps.push(`fill ${step.selector} ${JSON.stringify(value)}`);
      } else if (step.kind === "click" && step.selector) {
        await page.click(step.selector, { timeout: 8000 });
        steps.push(`click ${step.selector}${step.text ? ` (${step.text})` : ""}`);
      }
      // "request" steps replay themselves as a consequence of the UI actions.
    }
    await page.waitForTimeout(2000); // let in-flight requests land
  } catch (err) {
    await browser.close();
    return { completed: false, error: String(err.message).slice(0, 300), steps, responses, pageErrors };
  }

  let originalFailureObserved = false;
  if (failure.kind === "friction") {
    // The measured probe: click the element the user fought with, and watch
    // for ANY observable effect — a request, a DOM change, a navigation.
    let effect;
    try {
      await page.evaluate(() => {
        window.__drums_m = 0;
        new MutationObserver((ms) => (window.__drums_m += ms.length)).observe(
          document.documentElement,
          { subtree: true, childList: true, attributes: true, characterData: true }
        );
      });
      let probeRequests = 0;
      const onReq = (r) => { if (["xhr", "fetch"].includes(r.resourceType())) probeRequests++; };
      page.on("request", onReq);
      const href0 = page.url();
      for (let i = 0; i < 3; i++) {
        await page.click(failure.selector, { timeout: 8000 });
        await page.waitForTimeout(300);
      }
      steps.push(`click ${failure.selector} x3 (measured probe)`);
      await page.waitForTimeout(1500);
      page.off("request", onReq);
      const m = await page.evaluate(() => window.__drums_m).catch(() => 0);
      effect = { requests: probeRequests, domMutations: m, navigated: page.url() !== href0 };
    } catch (err) {
      await browser.close();
      return { completed: false, error: String(err.message).slice(0, 300), steps, responses, pageErrors };
    }
    const effectObserved = effect.requests > 0 || effect.domMutations > 0 || effect.navigated;
    originalFailureObserved = !effectObserved; // still dead = still failing
    const otherFailures = responses
      .filter((r) => r.status >= 500)
      .map((r) => `${r.method} ${r.path} -> ${r.status}`)
      .concat(pageErrors.map((e) => `pageerror: ${e}`));
    const bodyText = await page.evaluate(() => document.body.innerText).catch(() => "");
    if (screenshotPath) await page.screenshot({ path: screenshotPath }).catch(() => {});
    await browser.close();
    return { completed: true, originalFailureObserved, otherFailures, steps, responses, pageErrors, effect, pageText: bodyText.slice(0, 500) };
  }
  if (failure.kind === "http") {
    originalFailureObserved = responses.some(
      (r) => r.method === failure.method && r.path === failure.path && r.status >= 500
    );
  } else if (failure.kind === "jserror") {
    const needle = String(failure.message || "").slice(0, 80);
    originalFailureObserved = needle.length > 0 && pageErrors.some((e) => e.includes(needle));
  }

  const otherFailures = responses
    .filter((r) => r.status >= 500 && !(failure.kind === "http" && r.method === failure.method && r.path === failure.path))
    .map((r) => `${r.method} ${r.path} -> ${r.status}`)
    .concat(failure.kind === "jserror" ? [] : pageErrors.map((e) => `pageerror: ${e}`));

  const bodyText = await page.evaluate(() => document.body.innerText).catch(() => "");
  if (screenshotPath) await page.screenshot({ path: screenshotPath }).catch(() => {});
  await browser.close();
  return { completed: true, originalFailureObserved, otherFailures, steps, responses, pageErrors, pageText: bodyText.slice(0, 500) };
}

module.exports = { replayIncident };
