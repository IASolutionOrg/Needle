import { QueryClient, QueryClientProvider } from "@tanstack/react-query"
import { render, screen } from "@testing-library/react"
import { vi } from "vitest"

import App from "@/App"
import { TooltipProvider } from "@/components/ui/tooltip"

const controlPlane = {
  schema: "needle.control-plane/1",
  runtime: {
    status: "healthy",
    transport: "codex-app-server",
    sandbox: "read-only",
    approval_policy: "on-request",
    storage: "sqlite",
    external_telemetry: false,
  },
  phases: [],
  routes: [
    {
      id: "locate.implementation",
      enabled: true,
      priority: 100,
      preset_id: "locate.implementation",
      definition_digest: "b3:route",
    },
  ],
  plans: [],
  settings: null,
  cache: [],
  worker_runs: 0,
  pending_approvals: 0,
  route_promotions: [],
  changes: [],
  cost_observations: [],
}

class EventSourceStub {
  addEventListener() {}
  removeEventListener() {}
  close() {}
}

test("overview distinguishes proof metrics without inventing cost data", async () => {
  vi.stubGlobal("EventSource", EventSourceStub)
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input)
      const body = url.includes("/approvals") ? [] : controlPlane
      return new Response(JSON.stringify(body), {
        status: 200,
        headers: { "content-type": "application/json" },
      })
    }),
  )
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  })
  render(
    <QueryClientProvider client={queryClient}>
      <TooltipProvider>
        <App />
      </TooltipProvider>
    </QueryClientProvider>,
  )

  expect(await screen.findByRole("heading", { name: "Overview" })).toBeVisible()
  expect(screen.getAllByText("Recorded plans")).toHaveLength(8)
  expect(screen.getByText("0 candidates")).toBeVisible()
  expect(screen.getByText("0 active")).toBeVisible()
  expect(screen.getByText("No cost data yet")).toBeVisible()
  expect(screen.queryByText(/\$\d/)).not.toBeInTheDocument()
})

test("changes page lists isolated patches without exposing their full diff", async () => {
  window.history.pushState({}, "", "/changes")
  vi.stubGlobal("EventSource", EventSourceStub)
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input)
      const body = url.endsWith("/api/v1/changes")
        ? [
            {
              change_id: "chg_0123456789abcdef01234567",
              state: "verified",
              patch_id: `b3:${"1".repeat(64)}`,
              revision: 1,
              summary: "Update the fixture",
              changed_files: [{ path: "fixture.txt", operation: "update" }],
              change_digest: `b3:${"2".repeat(64)}`,
              created_unix_ms: 1,
            },
          ]
        : url.includes("/approvals")
          ? []
          : controlPlane
      return new Response(JSON.stringify(body), {
        status: 200,
        headers: { "content-type": "application/json" },
      })
    })
  )
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  })
  render(
    <QueryClientProvider client={queryClient}>
      <TooltipProvider>
        <App />
      </TooltipProvider>
    </QueryClientProvider>
  )

  expect(await screen.findByRole("heading", { name: "Changes" })).toBeVisible()
  expect(await screen.findByText("chg_0123456789abcdef01234567")).toBeVisible()
  expect(screen.getByText("Update the fixture")).toBeVisible()
  expect(screen.queryByText("Filesystem-derived diff")).not.toBeInTheDocument()
  window.history.pushState({}, "", "/")
})
