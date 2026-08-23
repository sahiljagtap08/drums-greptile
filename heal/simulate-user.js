// A stand-in for the real user during testing: opens the app in a real
// browser and hits the bug the way a person would. At demo time a human does
// this by hand; nothing in the loop depends on this script.
const { chromium } = require("playwright");

(async () => {
  const mode = process.argv[2] === "invite" ? "invite" : "signup";
  const base = process.argv[3] || "http://localhost:3000";
  const browser = await chromium.launch();
  const page = await browser.newPage();
  await page.goto(base + "/");
  if (mode === "signup") {
    await page.fill("#email", "sahil+test@gmail.com");
    await page.click("#join");
    await page.waitForTimeout(2500); // let the snippet report
    console.log("user saw:", (await page.locator("#msg").textContent()) || "(nothing)");
  } else {
    // a user fighting a dead button: click, wait, click again, again, again
    for (let i = 0; i < 4; i++) {
      await page.click("#invite");
      await page.waitForTimeout(900);
    }
    await page.waitForTimeout(2500); // let the snippet report
    console.log("user saw:", (await page.locator("#code").textContent()) || "(nothing — the button did nothing)");
  }
  await browser.close();
})();
