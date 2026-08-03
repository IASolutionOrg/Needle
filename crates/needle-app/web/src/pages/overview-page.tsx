import {
  Activity,
  Box,
  Database,
  Inbox,
  LockKeyhole,
  Network,
  Server,
} from "lucide-react"
import type { ReactNode } from "react"
import { Link } from "react-router-dom"

import { useControlPlane } from "@/api"
import { Empty, EmptyDescription, EmptyHeader, EmptyTitle } from "@/components/ui/empty"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"

const resolutions = [
  "Exact Hit",
  "Coverage Hit",
  "Composite Hit",
  "Partial Hit",
  "Miss",
  "Stale",
  "Ambiguous",
  "Contradicted",
  "Rejected",
  "Bypass",
]

function Panel({
  title,
  children,
  className = "",
}: {
  title: string
  children: ReactNode
  className?: string
}) {
  return (
    <section className={`border bg-panel ${className}`}>
      <div className="border-b px-4 py-3">
        <h2 className="text-sm font-semibold">{title}</h2>
      </div>
      {children}
    </section>
  )
}

export default function OverviewPage() {
  const control = useControlPlane()
  const routes = control.data?.routes ?? []
  const semantic = control.data?.semantic
  const proofPlans = semantic?.selected_plans ?? []
  const proofMetrics = semantic?.metrics

  return (
    <div className="grid min-h-[calc(100svh-3.75rem)] grid-cols-1 xl:grid-cols-[minmax(0,1fr)_18rem]">
      <div className="min-w-0 p-4 md:p-6">
        <h1 className="mb-5 text-2xl font-semibold tracking-tight">Overview</h1>

        <section className="mb-5 grid border bg-panel sm:grid-cols-2 xl:grid-cols-4">
          {[
            [Database, "SQLite ready"],
            [Server, "App Server compatible"],
            [LockKeyhole, "Sandbox read-only"],
            [Network, "Approval bridge ready"],
          ].map(([Icon, label], index) => (
            <div
              key={String(label)}
              className={`flex h-16 items-center gap-3 px-4 ${
                index > 0 ? "border-t sm:border-t-0 sm:border-l" : ""
              }`}
            >
              <Icon className="text-primary" />
              <span className="text-sm">{String(label)}</span>
            </div>
          ))}
        </section>

        <section className="mb-5 grid border bg-panel sm:grid-cols-2 xl:grid-cols-4">
          {[
            ["Legacy v0.3 entries", control.data?.cache.length ?? 0],
            ["Proof candidates", proofPlans.length],
            ["Workers avoided", proofMetrics?.worker_avoided ?? 0],
            ["Proof overhead", `${proofMetrics?.proof_overhead_micros ?? 0} µs`],
          ].map(([label, value], index) => (
            <div
              key={String(label)}
              className={`px-4 py-3 ${
                index > 0 ? "border-t sm:border-t-0 sm:border-l" : ""
              }`}
            >
              <div className="text-xs text-muted-foreground">{String(label)}</div>
              <div className="mt-1 font-mono text-lg">{String(value)}</div>
            </div>
          ))}
        </section>

        <div className="grid gap-4 lg:grid-cols-2">
          <Panel title="Cache resolution">
            <div className="divide-y">
              {resolutions.map((resolution) => (
                <div
                  key={resolution}
                  className="grid grid-cols-[1fr_auto_auto] items-center gap-4 px-4 py-2.5 text-[13px]"
                >
                  <span>{resolution}</span>
                  <span className="font-mono text-muted-foreground">
                    {
                      proofPlans.filter((plan) =>
                        plan.decision_reason.includes(resolution.replaceAll(" ", ""))
                      ).length
                    }
                  </span>
                  <span className="text-muted-foreground">
                    {resolution === "Stale"
                      ? `${proofMetrics?.stale_candidates ?? 0} candidates`
                      : resolution === "Contradicted"
                        ? `${proofMetrics?.active_contradictions ?? 0} active`
                        : "Recorded plans"}
                  </span>
                </div>
              ))}
            </div>
          </Panel>

          <Panel title="Cost trend">
            <Empty className="min-h-78 rounded-none border-0">
              <EmptyHeader>
                <Box />
                <EmptyTitle>No cost data yet</EmptyTitle>
                <EmptyDescription>
                  Run an approved experiment to begin measurement
                </EmptyDescription>
              </EmptyHeader>
            </Empty>
          </Panel>

          <Panel title="Route health" className="lg:col-span-2">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Route</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead>Quality</TableHead>
                  <TableHead>p95</TableHead>
                  <TableHead>Error rate</TableHead>
                  <TableHead>Last run</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {routes.map((route) => (
                  <TableRow key={route.id}>
                    <TableCell className="font-mono text-link">{route.id}</TableCell>
                    <TableCell>Not promoted</TableCell>
                    <TableCell>—</TableCell>
                    <TableCell>—</TableCell>
                    <TableCell>—</TableCell>
                    <TableCell>—</TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </Panel>

          <Panel title="Recent runs" className="lg:col-span-2">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Route</TableHead>
                  <TableHead>Resolution</TableHead>
                  <TableHead>Workers</TableHead>
                  <TableHead>Quality</TableHead>
                  <TableHead>Duration</TableHead>
                  <TableHead>Cost</TableHead>
                </TableRow>
              </TableHeader>
            </Table>
            <Empty className="min-h-44 rounded-none border-0">
              <EmptyHeader>
                <Inbox />
                <EmptyTitle>No runs recorded</EmptyTitle>
              </EmptyHeader>
            </Empty>
          </Panel>
        </div>
      </div>

      <aside className="border-t bg-sidebar p-4 xl:border-t-0 xl:border-l">
        <div className="mb-3 flex items-center justify-between">
          <h2 className="text-sm font-semibold">Approvals</h2>
          <Link className="text-xs text-link hover:underline" to="/approvals">
            Review
          </Link>
        </div>
        <Empty className="min-h-52 rounded-none border-0">
          <EmptyHeader>
            <Inbox />
            <EmptyTitle>No pending approvals</EmptyTitle>
          </EmptyHeader>
        </Empty>
        <div className="mt-4 border-t pt-4">
          <h2 className="mb-4 text-sm font-semibold">Runtime events</h2>
          <div className="flex items-center gap-3 text-xs">
            <Activity className="text-success" />
            <span>Runtime started</span>
          </div>
        </div>
      </aside>
    </div>
  )
}
