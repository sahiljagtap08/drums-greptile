// Replay turns user evidence into an executable reproduction. The same
// function is the reproducer (against HEAD) and the verifier (against the
// candidate): it performs the user's recorded actions and reports what it
// observed. It never decides "fixed" — the pipeline does.
const { chromium } = require("playwright");

async function replayIncident(incident, baseURL) {
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

  const steps = [];
  try {
    await page.goto(baseURL + (incident.url || "/"), { waitUntil: "load", timeout: 15000 });
    steps.push(`open ${incident.url || "/"}`);
    for (const step of incident.trace || []) {
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

  const failure = incident.failure || {};
  let originalFailureObserved = false;
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
  await browser.close();
  return { completed: true, originalFailureObserved, otherFailures, steps, responses, pageErrors, pageText: bodyText.slice(0, 500) };
}

module.exports = { replayIncident };
