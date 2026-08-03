import { useEffect, useMemo, useState, type ReactNode } from "react"
import { AlertTriangle, Check, Clock3, Copy, ShieldCheck, X } from "lucide-react"

import {
  type ApprovalDecision,
  type ApprovalRequest,
  useApprovalDecision,
  useApprovals,
} from "@/api"
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Empty, EmptyDescription, EmptyHeader, EmptyTitle } from "@/components/ui/empty"
import { ScrollArea } from "@/components/ui/scroll-area"
import { Separator } from "@/components/ui/separator"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs"

function timeRemaining(expires: number, now: number) {
  const seconds = Math.max(0, Math.floor((expires - now) / 1000))
  const minutes = Math.floor(seconds / 60)
  return `${String(minutes).padStart(2, "0")}:${String(seconds % 60).padStart(2, "0")}`
}

function compactDigest(value: string) {
  return value.length > 24 ? `${value.slice(0, 16)}…${value.slice(-8)}` : value
}

export default function ApprovalInboxPage() {
  const approvals = useApprovals()
  const decision = useApprovalDecision()
  const [selectedId, setSelectedId] = useState<string | null>(null)
  const [confirmation, setConfirmation] = useState<{
    request: ApprovalRequest
    decision: ApprovalDecision
  } | null>(null)
  const [now, setNow] = useState(0)

  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1000)
    return () => window.clearInterval(timer)
  }, [])

  const pending = useMemo(() => approvals.data ?? [], [approvals.data])
  const selected = useMemo(
    () => pending.find((request) => request.id === selectedId) ?? pending[0] ?? null,
    [pending, selectedId],
  )

  const confirm = async () => {
    if (!confirmation) return
    await decision.mutateAsync(confirmation)
    setConfirmation(null)
  }

  return (
    <div className="grid min-h-[calc(100svh-3.75rem)] grid-cols-1 xl:grid-cols-[minmax(28rem,1.05fr)_minmax(24rem,0.95fr)]">
      <section className="min-w-0 border-r">
        <div className="px-5 pt-6 md:px-6">
          <h1 className="text-2xl font-semibold tracking-tight">Approval Inbox</h1>
          <Tabs defaultValue="pending" className="mt-5">
            <TabsList variant="line">
              <TabsTrigger value="pending">
                Pending <span className="font-mono">{pending.length}</span>
              </TabsTrigger>
              <TabsTrigger value="resolved" disabled>
                Resolved
              </TabsTrigger>
              <TabsTrigger value="timed-out" disabled>
                Timed out
              </TabsTrigger>
            </TabsList>
          </Tabs>
        </div>
        <Separator />
        {pending.length === 0 ? (
          <Empty className="min-h-96 rounded-none border-0">
            <EmptyHeader>
              <ShieldCheck />
              <EmptyTitle>No pending approvals</EmptyTitle>
              <EmptyDescription>
                Unknown commands appear here until accepted once, declined, or timed out.
              </EmptyDescription>
            </EmptyHeader>
          </Empty>
        ) : (
          <ScrollArea className="h-[calc(100svh-10.75rem)]">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Status</TableHead>
                  <TableHead>Command</TableHead>
                  <TableHead>Route</TableHead>
                  <TableHead>Repository</TableHead>
                  <TableHead>Expires</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {pending.map((request) => (
                  <TableRow
                    key={request.id}
                    data-state={selected?.id === request.id ? "selected" : undefined}
                    className="cursor-pointer"
                    onClick={() => setSelectedId(request.id)}
                  >
                    <TableCell>
                      <span className="inline-flex items-center gap-2 text-pending">
                        <span className="size-2 rounded-full bg-pending" />
                        Pending
                      </span>
                    </TableCell>
                    <TableCell className="max-w-64 truncate font-mono">
                      {request.command_display ?? request.argv.join(" ")}
                    </TableCell>
                    <TableCell className="font-mono text-link">{request.route}</TableCell>
                    <TableCell className="font-mono">
                      {compactDigest(request.repository_id)}
                    </TableCell>
                    <TableCell className="font-mono text-pending">
                      {timeRemaining(request.expires_unix_ms, now)}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </ScrollArea>
        )}
      </section>

      <ApprovalDetail
        request={selected}
        now={now}
        pending={decision.isPending}
        onDecision={(request, nextDecision) =>
          setConfirmation({ request, decision: nextDecision })
        }
      />

      <AlertDialog
        open={confirmation !== null}
        onOpenChange={(open) => {
          if (!open) setConfirmation(null)
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>
              {confirmation?.decision === "accept"
                ? "Accept this exact command once?"
                : confirmation?.decision === "cancel"
                  ? "Cancel the worker turn?"
                  : "Decline this command?"}
            </AlertDialogTitle>
            <AlertDialogDescription>
              The decision is bound to the displayed approval ID and payload digest. It
              cannot be replayed if argv, cwd, or permissions change.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Keep pending</AlertDialogCancel>
            <AlertDialogAction onClick={confirm}>Confirm decision</AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  )
}

function ApprovalDetail({
  request,
  now,
  pending,
  onDecision,
}: {
  request: ApprovalRequest | null
  now: number
  pending: boolean
  onDecision: (request: ApprovalRequest, decision: ApprovalDecision) => void
}) {
  if (!request) {
    return (
      <Empty className="min-h-[calc(100svh-3.75rem)] rounded-none border-0">
        <EmptyHeader>
          <Clock3 />
          <EmptyTitle>Approval details</EmptyTitle>
          <EmptyDescription>Select a pending request to inspect it.</EmptyDescription>
        </EmptyHeader>
      </Empty>
    )
  }

  return (
    <section className="min-w-0">
      <div className="border-b px-5 py-4 md:px-6">
        <h2 className="text-sm font-semibold">Approval details</h2>
      </div>
      <ScrollArea className="h-[calc(100svh-7.75rem)]">
        <div className="flex flex-col gap-5 p-5 text-[13px] md:p-6">
          <Definition label="Approval ID" value={request.id} copy />
          <Definition label="Payload digest" value={request.payload_digest} copy />
          <Definition
            label="Thread / Turn / Item"
            value={`${request.thread_id} / ${request.turn_id} / ${request.item_id}`}
          />
          <Definition label="Classification">
            <Badge variant="outline" className="border-pending text-pending">
              Pending user
            </Badge>
          </Definition>
          <Definition label="Command (direct argv)">
            <div className="flex max-w-full gap-1 overflow-x-auto pb-2">
              {request.argv.map((argument, index) => (
                <code key={`${argument}-${index}`} className="border bg-muted px-2 py-1">
                  {argument}
                </code>
              ))}
            </div>
          </Definition>
          <Definition label="cwd (inside checkout)" value={request.cwd} />
          <Definition label="Reason" value={request.reason ?? "No reason supplied"} />
          <Definition label="Requested permissions">
            <ul className="list-inside list-disc border bg-muted/30 p-3">
              <li>Read repository files</li>
              <li>
                Write paths:{" "}
                {request.requested_permissions.write_paths.length > 0
                  ? request.requested_permissions.write_paths.join(", ")
                  : "none"}
              </li>
              <li>Network: {request.requested_permissions.network ? "requested" : "none"}</li>
            </ul>
          </Definition>
          <Definition label="Route" value={request.route} />
          <Definition label="Repository" value={request.repository_id} />
          <Definition
            label="Expires in"
            value={timeRemaining(request.expires_unix_ms, now)}
            accent
          />

          <div className="flex items-start gap-3 border border-pending bg-pending/5 p-3 text-pending">
            <AlertTriangle className="mt-0.5 shrink-0" />
            <span>Any change to argv, cwd, or permissions invalidates this decision.</span>
          </div>

          <div className="grid gap-2 sm:grid-cols-3">
            <Button
              disabled={pending}
              onClick={() => onDecision(request, "accept")}
            >
              <Check data-icon="inline-start" />
              Accept once
            </Button>
            <Button
              variant="destructive"
              disabled={pending}
              onClick={() => onDecision(request, "decline")}
            >
              <X data-icon="inline-start" />
              Decline
            </Button>
            <Button
              variant="outline"
              disabled={pending}
              onClick={() => onDecision(request, "cancel")}
            >
              Cancel turn
            </Button>
          </div>

          <Separator />
          <div>
            <h3 className="mb-4 text-sm font-semibold">Approval timeline</h3>
            <ol className="border-l pl-5">
              <li className="relative pb-5">
                <span className="absolute top-0.5 -left-[1.6rem] size-3 rounded-full bg-pending" />
                <div className="font-medium text-pending">Requested</div>
                <div className="text-muted-foreground">
                  Command requested by Codex App Server
                </div>
              </li>
              <li className="relative pb-5">
                <span className="absolute top-0.5 -left-[1.6rem] size-3 rounded-full border border-pending bg-background" />
                <div className="font-medium text-pending">Awaiting decision</div>
                <div className="text-muted-foreground">
                  {timeRemaining(request.expires_unix_ms, now)} remaining
                </div>
              </li>
              <li className="relative">
                <span className="absolute top-0.5 -left-[1.6rem] size-3 rounded-full border bg-background" />
                <div className="text-muted-foreground">Timeout</div>
              </li>
            </ol>
          </div>

          <Separator />
          <div>
            <h3 className="mb-3 text-sm font-semibold">Auto-approval policy</h3>
            <div className="grid gap-2 border p-3 sm:grid-cols-2">
              {[
                "Trusted direct cargo test only",
                "Exact TestPlan argv",
                "Isolated target/temp writes only",
                "No network",
                "Maximum 2 executions",
              ].map((item) => (
                <div key={item} className="flex items-center gap-2">
                  <Check className="text-success" />
                  {item}
                </div>
              ))}
            </div>
          </div>
        </div>
      </ScrollArea>
    </section>
  )
}

function Definition({
  label,
  value,
  copy = false,
  accent = false,
  children,
}: {
  label: string
  value?: string
  copy?: boolean
  accent?: boolean
  children?: ReactNode
}) {
  return (
    <div className="grid gap-2 border-b pb-4 sm:grid-cols-[9rem_minmax(0,1fr)]">
      <dt className="text-muted-foreground">{label}</dt>
      <dd
        className={`min-w-0 break-all font-mono ${
          accent ? "text-pending" : "text-link"
        }`}
      >
        {children ?? value}
        {copy && value ? (
          <button
            type="button"
            className="ml-2 inline-flex text-muted-foreground hover:text-foreground"
            onClick={() => navigator.clipboard.writeText(value)}
            aria-label={`Copy ${label}`}
          >
            <Copy />
          </button>
        ) : null}
      </dd>
    </div>
  )
}
