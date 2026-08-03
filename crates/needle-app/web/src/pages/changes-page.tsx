import { type ReactNode, useState } from "react"
import { ArrowLeft, FilePenLine } from "lucide-react"
import { Link, useParams } from "react-router-dom"

import {
  type ChangeAttempt,
  useApplyChange,
  useChange,
  useChangeDiff,
  useChanges,
} from "@/api"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"

export default function ChangesPage() {
  const { id } = useParams()
  return id ? <ChangeDetailPage changeId={id} /> : <ChangeListPage />
}

function PageHeader({ children }: { children?: ReactNode }) {
  return (
    <div className="mb-6 flex items-start justify-between gap-4">
      <div className="flex items-start gap-3">
        <FilePenLine className="mt-1 text-primary" />
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Changes</h1>
          <p className="mt-1 text-sm text-muted-foreground">
            Isolated patches, independent verification, repair, and explicit apply.
          </p>
        </div>
      </div>
      {children}
    </div>
  )
}

function ChangeListPage() {
  const changes = useChanges()
  return (
    <div className="p-4 md:p-6">
      <PageHeader />
      {changes.error ? (
        <p className="border border-destructive/40 p-4 text-sm text-destructive">
          {changes.error.message}
        </p>
      ) : (
        <section className="border bg-panel">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Change</TableHead>
                <TableHead>State</TableHead>
                <TableHead>Revision</TableHead>
                <TableHead>Files</TableHead>
                <TableHead>Summary</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {(changes.data ?? []).map((change) => (
                <TableRow key={change.change_id}>
                  <TableCell className="font-mono">
                    <Link className="text-primary hover:underline" to={`/changes/${change.change_id}`}>
                      {change.change_id}
                    </Link>
                  </TableCell>
                  <TableCell>
                    <Badge variant="outline">{change.state}</Badge>
                  </TableCell>
                  <TableCell>{change.revision}</TableCell>
                  <TableCell>{change.changed_files.length}</TableCell>
                  <TableCell className="max-w-96 truncate">{change.summary}</TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
          {!changes.isLoading && changes.data?.length === 0 ? (
            <p className="p-8 text-center text-sm text-muted-foreground">
              No prepared changes are recorded.
            </p>
          ) : null}
        </section>
      )}
    </div>
  )
}

function ChangeDetailPage({ changeId }: { changeId: string }) {
  const detail = useChange(changeId)
  const diff = useChangeDiff(changeId)
  const apply = useApplyChange()
  const [confirmed, setConfirmed] = useState(false)
  const record = detail.data
  const applied = record?.applies.some((item) => item.status === "applied") ?? false

  return (
    <div className="p-4 md:p-6">
      <PageHeader>
        <Button asChild variant="outline">
          <Link to="/changes">
            <ArrowLeft /> Back
          </Link>
        </Button>
      </PageHeader>
      {detail.error ? (
        <p className="border border-destructive/40 p-4 text-sm text-destructive">
          {detail.error.message}
        </p>
      ) : record ? (
        <div className="grid gap-4">
          <section className="border bg-panel p-4">
            <div className="flex flex-wrap items-center justify-between gap-3">
              <div>
                <div className="font-mono text-sm">{record.change.patch.change_id}</div>
                <h2 className="mt-2 text-lg font-semibold">{record.change.patch.summary}</h2>
              </div>
              <div className="flex gap-2">
                <Badge variant="outline">revision {record.change.patch.revision}</Badge>
                <Badge variant="outline">{record.change.state}</Badge>
              </div>
            </div>
            <p className="mt-4 text-sm">{record.change.request.task}</p>
            <div className="mt-4 grid gap-4 lg:grid-cols-2">
              <List title="Acceptance criteria" values={record.change.request.acceptance_criteria} />
              <List title="Residual risks" values={record.change.patch.residual_risks} empty="None recorded" />
            </div>
          </section>

          <section className="border bg-panel">
            <h2 className="border-b px-4 py-3 text-sm font-semibold">Filesystem-derived diff</h2>
            <pre className="max-h-[32rem] overflow-auto p-4 font-mono text-xs leading-relaxed">
              {diff.data ?? (diff.isLoading ? "Loading diff…" : "Diff unavailable")}
            </pre>
          </section>

          <section className="border bg-panel p-4">
            <div className="flex flex-wrap items-center justify-between gap-3">
              <div>
                <h2 className="text-sm font-semibold">Independent verification</h2>
                <p className="mt-1 text-sm text-muted-foreground">
                  {record.verification
                    ? `${record.verification.verdict} · ${record.verification.test_evidence_ids.length} certified test evidence item(s)`
                    : "Not requested"}
                </p>
              </div>
              <Badge variant="outline">{record.verification?.verdict ?? "not_requested"}</Badge>
            </div>
            {record.verification?.test_plans_over_cap ? (
              <p className="mt-3 text-sm text-destructive">
                The certified plan set exceeded the verifier bound; no test subset was executed.
              </p>
            ) : null}
            {record.verification?.test_plan_results?.length ? (
              <div className="mt-4 overflow-x-auto">
                <h3 className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
                  Certified plans
                </h3>
                <Table className="mt-2">
                  <TableHeader>
                    <TableRow>
                      <TableHead>Plan</TableHead>
                      <TableHead>Available</TableHead>
                      <TableHead>Executed</TableHead>
                      <TableHead>Passed</TableHead>
                      <TableHead>Evidence / reason</TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {record.verification.test_plan_results.map((plan) => (
                      <TableRow key={plan.plan_digest}>
                        <TableCell className="font-mono text-xs">{plan.test_identifier}</TableCell>
                        <TableCell>{plan.available ? "yes" : "no"}</TableCell>
                        <TableCell>{plan.executed ? "yes" : "no"}</TableCell>
                        <TableCell>{plan.passed ? "yes" : "no"}</TableCell>
                        <TableCell className="max-w-80 truncate text-xs">
                          {plan.evidence_id ?? plan.failure_reason ?? "—"}
                        </TableCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              </div>
            ) : null}
            <List title="Findings" values={record.verification?.findings ?? []} empty="No findings" />
          </section>

          <AttemptTable attempts={record.attempts} />

          <section className="border bg-panel p-4">
            <h2 className="text-sm font-semibold">Apply to active worktree</h2>
            <p className="mt-1 text-sm text-muted-foreground">
              Requires the latest verified revision and an unchanged base snapshot. Needle never stages,
              commits, merges, or pushes.
            </p>
            <label className="mt-4 flex items-center gap-2 text-sm">
              <input
                type="checkbox"
                checked={confirmed}
                disabled={!record.apply_allowed || applied || apply.isPending}
                onChange={(event) => setConfirmed(event.target.checked)}
              />
              I confirm applying this exact verified patch to the active worktree.
            </label>
            <Button
              className="mt-3"
              variant="destructive"
              disabled={!record.apply_allowed || !confirmed || applied || apply.isPending}
              onClick={() =>
                apply.mutate({ changeId, digest: record.change_digest })
              }
            >
              {applied ? "Applied" : apply.isPending ? "Applying…" : "Apply verified patch"}
            </Button>
            {apply.error ? (
              <p className="mt-2 text-sm text-destructive">{apply.error.message}</p>
            ) : null}
          </section>
        </div>
      ) : (
        <p className="text-sm text-muted-foreground">Loading change…</p>
      )}
    </div>
  )
}

function List({
  title,
  values,
  empty = "None",
}: {
  title: string
  values: string[]
  empty?: string
}) {
  return (
    <div className="mt-4">
      <h3 className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">{title}</h3>
      {values.length > 0 ? (
        <ul className="mt-2 list-disc space-y-1 pl-5 text-sm">
          {values.map((value) => <li key={value}>{value}</li>)}
        </ul>
      ) : (
        <p className="mt-2 text-sm text-muted-foreground">{empty}</p>
      )}
    </div>
  )
}

function AttemptTable({ attempts }: { attempts: ChangeAttempt[] }) {
  return (
    <section className="border bg-panel">
      <h2 className="border-b px-4 py-3 text-sm font-semibold">Patcher and verifier attempts</h2>
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>Role</TableHead>
            <TableHead>Patch</TableHead>
            <TableHead>Input / cached / output</TableHead>
            <TableHead>Duration</TableHead>
            <TableHead>Cost</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {attempts.map((attempt, index) => (
            <TableRow key={`${attempt.patch_id}-${attempt.role}-${index}`}>
              <TableCell>{attempt.role}</TableCell>
              <TableCell className="max-w-64 truncate font-mono">{attempt.patch_id}</TableCell>
              <TableCell className="font-mono">
                {attempt.usage.input_tokens ?? "—"} / {attempt.usage.cached_input_tokens ?? "—"} / {attempt.usage.output_tokens ?? "—"}
              </TableCell>
              <TableCell>{attempt.usage.duration_ms == null ? "—" : `${attempt.usage.duration_ms} ms`}</TableCell>
              <TableCell>{attempt.cost_microusd == null ? "Not priced" : `${attempt.cost_microusd} μUSD`}</TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </section>
  )
}
