// A stand-in for the real user during testing: opens the app in a real
// browser and hits the bug the way a person would. At demo time a human does
// this by hand; nothing in the loop depends on this script.
const { chromium } = require("playwright");

(async () => {
  const base = process.argv[2] || "http://localhost:3000";
  const email = process.argv[3] || "sahil+test@gmail.com";
  const browser = await chromium.launch();
  const page = await browser.newPage();
  await page.goto(base + "/");
  await page.fill("#email", email);
  await page.click("#join");
  await page.waitForTimeout(2500); // let the snippet report
  console.log("user saw:", (await page.locator("#msg").textContent()) || "(nothing)");
  await browser.close();
})();
