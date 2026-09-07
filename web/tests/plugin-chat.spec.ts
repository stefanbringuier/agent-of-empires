import { test, expect } from "./helpers/mockedTest";

test("plugin chat traps focus, resizes, and restores its launcher on Escape", async ({ page }) => {
  await page.route("**/api/login/status", (route) => route.fulfill({ json: { required: false, authenticated: true } }));
  await page.route("**/api/sessions", (route) => route.fulfill({ json: { sessions: [], workspace_ordering: [] } }));
  await page.route("**/api/plugins/ui-state", (route) => route.fulfill({ json: { entries: [], notifications: [] } }));
  await page.route("**/api/plugins/commands", (route) =>
    route.fulfill({
      json: {
        commands: [
          {
            fqid: "plugin.aoe.councilor.open",
            plugin_id: "aoe.councilor",
            id: "open",
            title: "Councilor",
            description: "",
            keybinds: ["Ctrl+O"],
            action: { kind: "open-chat" },
          },
        ],
      },
    }),
  );
  await page.route("**/api/plugins/commands/*/chat", (route) =>
    route.fulfill({ status: 503, body: "Configure a compatible agent" }),
  );
  for (const path of ["settings", "themes", "agents", "profiles", "groups", "devices", "docker/status", "about"]) {
    await page.route(`**/api/${path}`, (route) => route.fulfill({ json: path === "docker/status" ? {} : [] }));
  }
  await page.goto("/");
  const launcher = page.getByRole("button", { name: "Councilor", exact: true });
  await launcher.click();
  const dialog = page.getByRole("dialog", { name: "Councilor" });
  await expect(dialog).toBeVisible();
  await expect(dialog.getByRole("alert")).toContainText("Configure a compatible agent");
  const close = dialog.getByRole("button", { name: "Close" });
  await expect(close).toBeFocused();
  for (const key of ["Tab", "Tab", "Shift+Tab", "Shift+Tab"]) {
    await page.keyboard.press(key);
    await expect(close).toBeFocused();
  }
  await page.setViewportSize({ width: 390, height: 600 });
  await expect(dialog.getByRole("button", { name: "Close" })).toBeInViewport();
  await page.keyboard.press("Escape");
  await expect(dialog).not.toBeVisible();
  await expect(launcher).toBeFocused();
});
