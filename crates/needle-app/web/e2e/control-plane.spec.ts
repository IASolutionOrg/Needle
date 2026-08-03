import { expect, test } from "@playwright/test"

test("embedded control plane loads and navigates to the approval inbox", async ({
  page,
}) => {
  const launchUrl = process.env.NEEDLE_E2E_URL
  if (!launchUrl) {
    test.skip(true, "NEEDLE_E2E_URL is required")
    return
  }

  await page.goto(launchUrl)
  await expect(page.getByRole("heading", { name: "Overview" })).toBeVisible()
  const replay = await page.request.get(launchUrl)
  expect(replay.status()).toBe(401)

  await page.getByRole("link", { name: "Artifacts", exact: true }).click()
  await expect(page.getByRole("heading", { name: "Artifacts" })).toBeVisible()
  await expect(page.getByText("Validated artifact metadata")).toBeVisible()

  await page.getByRole("link", { name: "Cache", exact: true }).click()
  await expect(page.getByRole("heading", { name: "Cache" })).toBeVisible()
  await expect(page.getByText("Artifact request cache records")).toBeVisible()

  await page.getByRole("link", { name: "Settings", exact: true }).click()
  await expect(page.getByRole("heading", { name: "Settings" })).toBeVisible()
  await page.getByLabel("Timeout seconds").fill("240")
  await page.getByRole("button", { name: "Save settings" }).click()
  await expect(page.getByLabel("Timeout seconds")).toHaveValue("240")

  await page.getByRole("link", { name: "Models", exact: true }).click()
  await expect(page.getByRole("heading", { name: "Models" })).toBeVisible()
  await page
    .getByLabel("Profiles, one per line: model | reasoning | service tier")
    .fill("gpt-5.6-sol | high |")
  await page.getByRole("button", { name: "Save model policy" }).click()
  await expect(page.getByRole("cell", { name: "gpt-5.6-sol" })).toBeVisible()

  await page.getByRole("link", { name: "Approval Inbox", exact: true }).click()
  await expect(
    page.getByRole("heading", { name: "Approval Inbox" })
  ).toBeVisible()
  await expect(page.getByText("No pending approvals")).toBeVisible()
})
