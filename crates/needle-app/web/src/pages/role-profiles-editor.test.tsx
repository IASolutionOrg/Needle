import { QueryClient, QueryClientProvider } from "@tanstack/react-query"
import { fireEvent, render, screen, waitFor } from "@testing-library/react"
import { vi } from "vitest"

import RoleProfilesEditor from "@/pages/role-profiles-editor"

function renderEditor() {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  return render(
    <QueryClientProvider client={queryClient}>
      <RoleProfilesEditor />
    </QueryClientProvider>,
  )
}

function jsonResponse(value: unknown, status = 200) {
  return new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" },
  })
}

const detailDefinition = {
  profile_id: "explorer.default",
  role: "explorer",
  host: "codex",
  model: "gpt-5-mini",
  reasoning: "medium",
  service_tier: "default",
  timeout_seconds: 120,
  budget: { max_turns: 2, max_output_tokens: 1200, max_cost_microusd: 1000 },
  prompt_profile_digest: `b3:${"1".repeat(64)}`,
  output_contract_digest: `b3:${"2".repeat(64)}`,
  tool_policy: "read_only",
  command_policy: "read_only",
  filesystem_policy: "read_only_checkout",
  network_policy: "denied",
  test_policy: "disabled",
  repair_policy: "none",
  fallback_policy: "native",
  concurrency: 1,
  route_assignments: [],
  definition_digest: `b3:${"3".repeat(64)}`,
}

function detailResponse(overrides: Record<string, unknown> = {}) {
  return {
    schema: "needle.role-profiles/1",
    profile: {
      profile_id: "explorer.default",
      revision: 2,
      state: "draft",
      definition: detailDefinition,
      definition_digest: detailDefinition.definition_digest,
      worker_profile: null,
      worker_profile_digest: null,
      preflight: { passed: true, failures: [], worker_profile_digest: null },
      created_unix_ms: 1,
      activated_unix_ms: null,
      state_digest: `b3:${"4".repeat(64)}`,
      latest_revision: 2,
      latest_definition_digest: detailDefinition.definition_digest,
      active_revision: null,
      active_definition_digest: null,
      updated_unix_ms: 2,
      ...overrides,
    },
  }
}

function baseList() {
  return {
    schema: "needle.role-profiles/1",
    items: [
      {
        profile_id: "explorer.default",
        role: "explorer",
        host: "codex",
        latest_revision: 2,
        latest_definition_digest: detailDefinition.definition_digest,
        active_revision: null,
        active_definition_digest: null,
        state: "draft",
        state_digest: `b3:${"4".repeat(64)}`,
        updated_unix_ms: 2,
      },
    ],
    limit: 50,
  }
}

function baseRevisions() {
  return {
    schema: "needle.role-profiles/1",
    profile_id: "explorer.default",
    items: [
      { revision: 1, definition_digest: `b3:${"5".repeat(64)}`, role: "explorer", host: "codex", model: "gpt-5", reasoning: "medium", service_tier: "default", state: "draft", created_unix_ms: 1, activated_unix_ms: null },
      { revision: 2, definition_digest: detailDefinition.definition_digest, role: "explorer", host: "codex", model: "gpt-5-mini", reasoning: "medium", service_tier: "default", state: "draft", created_unix_ms: 2, activated_unix_ms: null },
    ],
    limit: 50,
    total: 2,
  }
}

function baseAudit() {
  return { schema: "needle.role-profiles/1", profile_id: "explorer.default", items: [], limit: 50 }
}

test("role profiles editor renders a bounded empty state", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn(async () =>
      new Response(JSON.stringify({ schema: "needle.role-profiles/1", items: [], limit: 50 }), {
        status: 200,
        headers: { "content-type": "application/json" },
      }),
    ),
  )
  renderEditor()
  expect(await screen.findByText("No profiles yet.")).toBeVisible()
  expect(screen.getByRole("button", { name: "Start a profile" })).toBeVisible()
})

test("role profiles editor surfaces list failures", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn(async () =>
      new Response(JSON.stringify({ code: "storage_error", message: "temporary failure" }), {
        status: 500,
        headers: { "content-type": "application/json" },
      }),
    ),
  )
  renderEditor()
  expect(await screen.findByText(/Unable to load role profiles: temporary failure/)).toBeVisible()
})

test("new profile form keeps unsupported host and network controls read-only", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn(async () =>
      new Response(JSON.stringify({ schema: "needle.role-profiles/1", items: [], limit: 50 }), {
        status: 200,
        headers: { "content-type": "application/json" },
      }),
    ),
  )
  renderEditor()
  fireEvent.click(await screen.findByRole("button", { name: "Start a profile" }))
  expect(screen.getByLabelText("Host (read-only Codex)")).toBeDisabled()
  expect(screen.getByLabelText("Network policy (fixed denied)")).toBeDisabled()
  expect(screen.getByText(/Non-Codex hosts are unavailable/)).toBeVisible()
})

test("role profiles editor keeps the bounded list loading state visible", () => {
  vi.stubGlobal("fetch", vi.fn(() => new Promise<Response>(() => undefined)))
  renderEditor()
  expect(screen.getByText(/Loading role profiles/)).toBeVisible()
})

test("role profiles editor surfaces bounded preflight failures and clears stale status on edits", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      if (String(input).endsWith("/preflight")) {
        return jsonResponse(
          {
            schema: "needle.role-profiles/1",
            operation: "invalid",
            passed: false,
            failures: ["model is not supported by the selected host"],
            definition: null,
            definition_digest: null,
            worker_profile: null,
            worker_profile_digest: null,
          },
          422,
        )
      }
      return jsonResponse({ schema: "needle.role-profiles/1", items: [], limit: 50 })
    }),
  )
  renderEditor()
  fireEvent.click(await screen.findByRole("button", { name: "Start a profile" }))
  fireEvent.click(screen.getByRole("button", { name: "Preflight" }))
  expect(await screen.findByText("Preflight failed")).toBeVisible()
  expect(screen.getByText("model is not supported by the selected host")).toBeVisible()
  fireEvent.change(screen.getByLabelText("Model"), { target: { value: "gpt-5-mini" } })
  expect(screen.queryByText("Preflight failed")).toBeNull()
})

test("role profiles editor switches revisions and keeps existing profile ids read-only", async () => {
  const revisionOne = {
    ...detailDefinition,
    model: "gpt-5",
    definition_digest: `b3:${"5".repeat(64)}`,
  }
  const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input)
    if (url.endsWith("/role-profiles")) return jsonResponse(baseList())
    if (url.includes("/revisions")) return jsonResponse(baseRevisions())
    if (url.includes("/audit")) return jsonResponse(baseAudit())
    if (url.includes("?revision=1")) return jsonResponse(detailResponse({ revision: 1, definition: revisionOne, definition_digest: revisionOne.definition_digest }))
    return jsonResponse(detailResponse())
  })
  vi.stubGlobal("fetch", fetchMock)
  renderEditor()
  expect(await screen.findByDisplayValue("gpt-5-mini")).toBeVisible()
  expect(screen.getByLabelText("Profile id")).toHaveAttribute("readonly")
  fireEvent.click(screen.getByRole("button", { name: /Revision 1/ }))
  expect(await screen.findByDisplayValue("gpt-5")).toBeVisible()
  fireEvent.click(screen.getByRole("button", { name: /Revision 2/ }))
  expect(await screen.findByDisplayValue("gpt-5-mini")).toBeVisible()
})

test("role profiles editor preserves explicit new-profile mode after auto-selection", async () => {
  const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input)
    if (url.endsWith("/role-profiles")) return jsonResponse(baseList())
    if (url.includes("/revisions")) return jsonResponse(baseRevisions())
    if (url.includes("/audit")) return jsonResponse(baseAudit())
    return jsonResponse(detailResponse())
  })
  vi.stubGlobal("fetch", fetchMock)
  renderEditor()
  await screen.findByDisplayValue("gpt-5-mini")
  const detailCallsBefore = fetchMock.mock.calls.filter(([input]) => {
    const url = String(input)
    return url.endsWith("/role-profiles/explorer.default") || url.includes("?revision=")
  }).length

  fireEvent.click(screen.getByRole("button", { name: "New profile" }))
  await waitFor(() => {
    expect(screen.getByRole("heading", { name: "New canonical profile" })).toBeVisible()
    expect(screen.getByLabelText("Profile id")).not.toHaveAttribute("readonly")
    expect(screen.getByLabelText("Profile id")).toHaveValue("explorer.default")
    expect(fetchMock.mock.calls.filter(([input]) => {
      const url = String(input)
      return url.endsWith("/role-profiles/explorer.default") || url.includes("?revision=")
    }).length).toBe(detailCallsBefore)
  })
})

test("role profiles editor invalidates activation and deactivation confirmations when the view revision changes", async () => {
  const activeDigest = detailDefinition.definition_digest
  const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input)
    if (url.endsWith("/role-profiles")) return jsonResponse(baseList())
    if (url.includes("/revisions")) return jsonResponse(baseRevisions())
    if (url.includes("/audit")) return jsonResponse(baseAudit())
    return jsonResponse(detailResponse({ state: "active", active_revision: 2, active_definition_digest: activeDigest }))
  })
  vi.stubGlobal("fetch", fetchMock)
  renderEditor()
  await screen.findByDisplayValue("gpt-5-mini")
  const activation = screen.getByLabelText("Confirm exact digest activation")
  const deactivation = screen.getByLabelText("Confirm current active digest deactivation")
  const activateButton = screen.getByRole("button", { name: "Activate selected revision" })
  const deactivateButton = screen.getByRole("button", { name: "Deactivate active revision" })
  fireEvent.click(activation)
  fireEvent.click(deactivation)
  expect(activation).toBeChecked()
  expect(deactivation).toBeChecked()
  expect(activateButton).toBeEnabled()
  expect(deactivateButton).toBeEnabled()

  fireEvent.click(screen.getByRole("button", { name: /Revision 1/ }))
  await screen.findByText("Revision and activation")
  expect(screen.getByLabelText("Confirm exact digest activation")).not.toBeChecked()
  expect(screen.getByLabelText("Confirm current active digest deactivation")).not.toBeChecked()
  expect(screen.getByRole("button", { name: "Activate selected revision" })).toBeDisabled()
  expect(screen.getByRole("button", { name: "Deactivate active revision" })).toBeDisabled()
})

test("role profiles editor refreshes after a stale draft write", async () => {
  const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input)
    if (url.endsWith("/role-profiles")) return jsonResponse(baseList())
    if (url.includes("/revisions")) return jsonResponse(baseRevisions())
    if (url.includes("/audit")) return jsonResponse(baseAudit())
    if (url.endsWith("/preflight")) {
      return jsonResponse({
        schema: "needle.role-profiles/1",
        profile_id: "explorer.default",
        operation: "revise",
        passed: true,
        failures: [],
        if_match: `b3:${"4".repeat(64)}`,
        definition: detailDefinition,
        definition_digest: detailDefinition.definition_digest,
        worker_profile: null,
        worker_profile_digest: null,
      })
    }
    if (url.endsWith("/draft")) {
      return jsonResponse({ code: "if_match_changed", message: "role-profile state changed" }, 412)
    }
    return jsonResponse(detailResponse())
  })
  vi.stubGlobal("fetch", fetchMock)
  renderEditor()
  await screen.findByDisplayValue("gpt-5-mini")
  fireEvent.click(screen.getByRole("button", { name: "Save draft" }))
  expect(await screen.findByText(/current data was refreshed/)).toBeVisible()
  await waitFor(() => {
    expect(fetchMock.mock.calls.filter(([input]) => String(input).endsWith("/role-profiles/explorer.default")).length).toBeGreaterThan(1)
  })
})

test("role profiles editor uses the loaded CAS and refreshes when preflight sees a newer state", async () => {
  const loadedState = `b3:${"4".repeat(64)}`
  const newerState = `b3:${"7".repeat(64)}`
  let draftCalls = 0
  const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input)
    if (url.endsWith("/role-profiles")) return jsonResponse(baseList())
    if (url.includes("/revisions")) return jsonResponse(baseRevisions())
    if (url.includes("/audit")) return jsonResponse(baseAudit())
    if (url.endsWith("/preflight")) {
      return jsonResponse({
        schema: "needle.role-profiles/1",
        profile_id: "explorer.default",
        operation: "revise",
        passed: true,
        failures: [],
        if_match: newerState,
        definition: detailDefinition,
        definition_digest: detailDefinition.definition_digest,
        worker_profile: null,
        worker_profile_digest: null,
      })
    }
    if (url.endsWith("/draft")) {
      draftCalls += 1
      return jsonResponse({ schema: "needle.role-profile-error/1", code: "unexpected", message: "draft should not be sent" }, 500)
    }
    return jsonResponse(detailResponse({ state_digest: loadedState }))
  })
  vi.stubGlobal("fetch", fetchMock)
  renderEditor()
  await screen.findByDisplayValue("gpt-5-mini")
  const detailCallsBefore = fetchMock.mock.calls.filter(([input]) => String(input).endsWith("/role-profiles/explorer.default")).length
  fireEvent.click(screen.getByRole("button", { name: "Save draft" }))
  expect(await screen.findByText(/changed while preflighting; current data was refreshed/)).toBeVisible()
  expect(draftCalls).toBe(0)
  await waitFor(() => {
    expect(fetchMock.mock.calls.filter(([input]) => String(input).endsWith("/role-profiles/explorer.default")).length).toBeGreaterThan(detailCallsBefore)
  })
})

test("role profiles editor confirms activation and sends the exact revision digest", async () => {
  let activationBody: Record<string, unknown> | undefined
  let activationHeaders: Headers | undefined
  const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input)
    if (url.endsWith("/role-profiles")) return jsonResponse(baseList())
    if (url.includes("/revisions")) return jsonResponse(baseRevisions())
    if (url.includes("/audit")) return jsonResponse(baseAudit())
    if (url.endsWith("/preflight")) {
      return jsonResponse({
        schema: "needle.role-profiles/1",
        profile_id: "explorer.default",
        operation: "activate",
        passed: true,
        failures: [],
        if_match: `b3:${"4".repeat(64)}`,
        definition: detailDefinition,
        definition_digest: detailDefinition.definition_digest,
        worker_profile: null,
        worker_profile_digest: null,
      })
    }
    if (url.endsWith("/activate")) {
      activationBody = JSON.parse(String(init?.body)) as Record<string, unknown>
      activationHeaders = new Headers(init?.headers)
      return jsonResponse({ ...detailResponse(), operation: "activate", state_digest: `b3:${"6".repeat(64)}` })
    }
    return jsonResponse(detailResponse())
  })
  vi.stubGlobal("fetch", fetchMock)
  renderEditor()
  await screen.findByDisplayValue("gpt-5-mini")
  const activateButton = screen.getByRole("button", { name: "Activate selected revision" })
  expect(activateButton).toBeDisabled()
  fireEvent.click(screen.getByLabelText("Confirm exact digest activation"))
  expect(activateButton).toBeEnabled()
  fireEvent.click(activateButton)
  await waitFor(() => expect(activationBody).toBeDefined())
  expect(activationBody).toEqual({
    revision: 2,
    definition_digest: detailDefinition.definition_digest,
    confirm: true,
  })
  expect(activationHeaders?.get("if-match")).toBe(`"${"b3:" + "4".repeat(64)}"`)
})
