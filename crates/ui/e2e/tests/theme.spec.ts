import { test, expect } from "../pages/fixtures";

// Behavior that only a real engine can observe: the inline boot stamp in
// <head> (helios-ui-chrome theme-boot.html) applies the stored preference
// before first paint; theme.js cycles it (system -> light -> dark), stores
// it in the shared `hfs_theme` cookie, migrates the old localStorage cache,
// and roams it via PATCH /_user/settings (RFC 7386). None of this is
// reachable by the tower::oneshot tests.

// "cycling roams..." below is the only test in this file that reaches the
// real settings store (the others intercept `/_user/settings` via
// `page.route`), so it's the only one that could leave the shared server's
// document holding a theme. An RFC 7386 `null` deletes the key, so this
// restores the "no stored preference" baseline a clean server starts at.
test.afterEach(async ({ request }) => {
  await request.patch("/_user/settings", {
    headers: { "Content-Type": "application/json" },
    data: { theme: null },
  });
});

test("a returning dark-mode user sees no flash of light", async ({ page, chrome }) => {
  await chrome.seedTheme("dark");
  // domcontentloaded, not networkidle: the attribute must already be right
  // before any script fetch resolves — that is the FOUC guard.
  await page.goto("/ui", { waitUntil: "domcontentloaded" });
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  await expect(page.locator("html")).toHaveAttribute("data-theme-pref", "dark");
});

test("the default is to follow the OS, live", async ({ browser }) => {
  const ctx = await browser.newContext({ colorScheme: "dark" });
  const page = await ctx.newPage();
  await page.goto("/ui", { waitUntil: "domcontentloaded" });
  await expect(page.locator("html")).toHaveAttribute("data-theme-pref", "system");
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  await page.emulateMedia({ colorScheme: "light" });
  await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  await ctx.close();
});

test("one button cycles system, light, dark and roams the choice", async ({ page, chrome }) => {
  await page.goto("/ui", { waitUntil: "networkidle" });
  const toggle = chrome.themeToggle();
  await expect(toggle).toHaveCount(1);
  await expect(toggle).toHaveAttribute("aria-label", "Theme: follow system");

  await toggle.click();
  await expect(page.locator("html")).toHaveAttribute("data-theme-pref", "light");
  await expect(toggle).toHaveAttribute("aria-label", "Theme: light");

  const patch = page.waitForRequest(
    (r) => r.url().endsWith("/_user/settings") && r.method() === "PATCH",
  );
  await toggle.click();
  const req = await patch;
  expect(JSON.parse(req.postData() ?? "{}")).toEqual({ theme: "dark" });
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  const cookies = await page.context().cookies();
  expect(cookies.find((c) => c.name === "hfs_theme")?.value).toBe("dark");

  await toggle.click();
  await expect(page.locator("html")).toHaveAttribute("data-theme-pref", "system");
});

test("a roamed server choice seeds a browser with no choice of its own", async ({ page }) => {
  await page.route("**/_user/settings", (route) =>
    route.request().method() === "GET"
      ? route.fulfill({ contentType: "application/json", body: JSON.stringify({ theme: "dark" }) })
      : route.fulfill({ status: 200, body: "{}" }),
  );
  await page.goto("/ui", { waitUntil: "networkidle" });
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
});

test("this browser's own choice wins over a roamed one", async ({ page, chrome }) => {
  await page.route("**/_user/settings", (route) =>
    route.request().method() === "GET"
      ? route.fulfill({ contentType: "application/json", body: JSON.stringify({ theme: "dark" }) })
      : route.fulfill({ status: 200, body: "{}" }),
  );
  await chrome.seedTheme("light");
  await page.goto("/ui", { waitUntil: "networkidle" });
  await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
});

test("the pre-cookie localStorage cache is migrated into the cookie", async ({ page, chrome }) => {
  await page.route("**/_user/settings", (route) => route.fulfill({ status: 503, body: "" }));
  await chrome.seedLegacyTheme("dark");
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  await page.goto("/ui", { waitUntil: "networkidle" });
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  const cookies = await page.context().cookies();
  expect(cookies.find((c) => c.name === "hfs_theme")?.value).toBe("dark");
  expect(errors, errors.join("\n")).toEqual([]);
});
