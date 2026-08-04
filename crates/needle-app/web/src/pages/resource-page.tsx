import {
  Bot,
  Boxes,
  Database,
  FlaskConical,
  GitBranch,
  Layers3,
  PlayCircle,
  Settings,
  SlidersHorizontal,
} from "lucide-react"
import { useState } from "react"
import { useParams } from "react-router"

import {
  type ControlPlane,
  type ModelPolicyInput,
  type SettingsConfig,
  useControlPlane,
  useModelPolicyUpdate,
  useRouteState,
  useSettingsUpdate,
} from "@/api"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import {
  Empty,
  EmptyDescription,
  EmptyHeader,
  EmptyTitle,
} from "@/components/ui/empty"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import RoleProfilesEditor from "@/pages/role-profiles-editor"

const resources = {
  platforms: {
    title: "Platforms",
    icon: Layers3,
    description: "Codex App Server compatibility and platform isolation.",
  },
  agents: {
    title: "Agents",
    icon: Bot,
    description: "Read-only worker profiles and execution boundaries.",
  },
  routes: {
    title: "Routes & Plans",
    icon: GitBranch,
    description: "Immutable bounded DAG definitions and route promotion state.",
  },
  artifacts: {
    title: "Artifacts",
    icon: Boxes,
    description:
      "Typed semantic artifacts, provenance, dependencies, and validations.",
  },
  cache: {
    title: "Cache",
    icon: Database,
    description:
      "Exact, composite, partial, stale, rejected, and bypass resolutions.",
  },
  runs: {
    title: "Runs",
    icon: PlayCircle,
    description:
      "Execution attempts, command evidence, usage, and approval timeline.",
  },
  models: {
    title: "Models",
    icon: SlidersHorizontal,
    description: "Fixed-order model ladder and promoted validated profiles.",
  },
  experiments: {
    title: "Experiments",
    icon: FlaskConical,
    description: "Approved paired experiments and bounded economic evidence.",
  },
  settings: {
    title: "Settings",
    icon: Settings,
    description:
      "Local runtime, worker, security, and retention configuration.",
  },
} as const

export default function ResourcePage() {
  const { resource = "platforms" } = useParams()
  const definition =
    resources[resource as keyof typeof resources] ?? resources.platforms
  const control = useControlPlane()
  const routeState = useRouteState()
  const Icon = definition.icon

  return (
    <div className="p-4 md:p-6">
      <div className="mb-6 flex items-start gap-3">
        <Icon className="mt-1 text-primary" />
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">
            {definition.title}
          </h1>
          <p className="mt-1 text-sm text-muted-foreground">
            {definition.description}
          </p>
        </div>
      </div>

      {resource === "platforms" ? (
        <div className="grid gap-4 lg:grid-cols-2">
          <SettingsPanel
            title="Codex adapter"
            rows={[
              ["Transport", control.data?.runtime.transport ?? "—"],
              ["Compatibility fixture", "Codex 0.144.0"],
              [
                "Approval protocol",
                control.data?.runtime.approval_policy ?? "—",
              ],
              ["IPC", "Named pipe / Unix socket"],
            ]}
          />
          <SettingsPanel
            title="Isolation"
            rows={[
              ["Source sandbox", control.data?.runtime.sandbox ?? "—"],
              ["Worker network", "Disabled"],
              ["Project instructions", "Disabled"],
              ["Plugins / MCP / multi-agent", "Disabled"],
            ]}
          />
        </div>
      ) : resource === "agents" ? (
        <div className="grid gap-4 lg:grid-cols-2">
          <SettingsPanel
            title="Discovery worker"
            rows={[
              [
                "Model",
                control.data?.settings?.worker_model ?? "Not configured",
              ],
              [
                "Reasoning",
                control.data?.settings?.worker_reasoning ?? "Not configured",
              ],
              [
                "Timeout",
                control.data?.settings
                  ? `${control.data.settings.worker_timeout_seconds}s`
                  : "Not configured",
              ],
              ["File changes", "Always declined"],
            ]}
          />
          <SettingsPanel
            title="Test execution"
            rows={[
              [
                "Trusted repository",
                control.data?.settings?.trusted_test_execution
                  ? "Enabled"
                  : "Disabled",
              ],
              ["Initial adapter", "Direct cargo test only"],
              ["Logical-worker budget", "2 executions"],
              ["Checkout", "Disposable exact snapshot"],
            ]}
          />
        </div>
      ) : resource === "routes" ? (
        <section className="border bg-panel">
          <div className="border-b px-4 py-3 text-sm font-semibold">
            Immutable definitions
          </div>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Route</TableHead>
                <TableHead>Revision</TableHead>
                <TableHead>Nodes</TableHead>
                <TableHead>Definition digest</TableHead>
                <TableHead>Promotion</TableHead>
                <TableHead className="text-right">State</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {(control.data?.plans ?? []).map((plan) => (
                <TableRow key={plan.id}>
                  <TableCell className="font-mono text-link">
                    {plan.id}
                  </TableCell>
                  <TableCell>{plan.revision}</TableCell>
                  <TableCell>{plan.nodes.length}</TableCell>
                  <TableCell className="max-w-72 truncate font-mono">
                    {plan.definition_digest}
                  </TableCell>
                  <TableCell>
                    <Badge variant="outline">
                      {control.data?.route_promotions.some(
                        (promotion) => promotion.route_key === plan.id
                      )
                        ? "Promoted"
                        : "Not promoted"}
                    </Badge>
                  </TableCell>
                  <TableCell className="text-right">
                    {control.data?.routes
                      .filter((route) => route.id === plan.id)
                      .map((route) => (
                        <Button
                          key={route.id}
                          size="sm"
                          variant="outline"
                          disabled={routeState.isPending}
                          onClick={() =>
                            routeState.mutate({
                              route,
                              enabled: !route.enabled,
                            })
                          }
                        >
                          {route.enabled ? "Disable" : "Enable"}
                        </Button>
                      ))}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </section>
      ) : resource === "models" ? (
        <div className="grid gap-5">
          <ModelPolicyPanel
            key={control.data?.model_policy_digest ?? "model-policy"}
            control={control.data ?? null}
          />
          <RoleProfilesEditor />
        </div>
      ) : resource === "artifacts" ? (
        <section className="border bg-panel">
          <div className="border-b px-4 py-3 text-sm font-semibold">
            Validated artifact metadata
          </div>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Kind</TableHead>
                <TableHead>Contract</TableHead>
                <TableHead>Scope</TableHead>
                <TableHead>Dependencies</TableHead>
                <TableHead>Validations</TableHead>
                <TableHead>Artifact ID</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {(control.data?.artifacts ?? []).map((artifact) => (
                <TableRow key={artifact.id}>
                  <TableCell>
                    <Badge variant="outline">{artifact.kind}</Badge>
                  </TableCell>
                  <TableCell className="font-mono">
                    {artifact.contract_id}
                  </TableCell>
                  <TableCell>{artifact.scope}</TableCell>
                  <TableCell>{artifact.dependency_count}</TableCell>
                  <TableCell>{artifact.validation_count}</TableCell>
                  <TableCell className="max-w-72 truncate font-mono">
                    {artifact.id}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
          {control.data?.artifacts.length === 0 ? (
            <RecordedDataEmpty icon={Boxes} />
          ) : null}
        </section>
      ) : resource === "cache" ? (
        <section className="border bg-panel">
          <div className="border-b px-4 py-3 text-sm font-semibold">
            Artifact request cache records
          </div>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Identity</TableHead>
                <TableHead>Source snapshot</TableHead>
                <TableHead>Hits</TableHead>
                <TableHead>Created</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {(control.data?.cache ?? []).map((entry) => (
                <TableRow key={entry.identity_digest}>
                  <TableCell className="max-w-72 truncate font-mono">
                    {entry.identity_digest}
                  </TableCell>
                  <TableCell className="max-w-72 truncate font-mono">
                    {entry.source_digest}
                  </TableCell>
                  <TableCell>{entry.hit_count}</TableCell>
                  <TableCell>
                    {new Date(entry.created_unix_ms).toLocaleString()}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
          {control.data?.cache.length === 0 ? (
            <RecordedDataEmpty icon={Database} />
          ) : null}
        </section>
      ) : resource === "runs" ? (
        <div className="grid gap-4">
          <div className="grid gap-4 lg:grid-cols-3">
            <MetricPanel
              label="Worker runs"
              value={control.data?.worker_runs ?? 0}
            />
            <MetricPanel
              label="Typed attempts"
              value={control.data?.execution_attempts ?? 0}
            />
            <MetricPanel
              label="Command evidence"
              value={control.data?.command_evidence ?? 0}
            />
          </div>
          <section className="border bg-panel">
            <h2 className="border-b px-4 py-3 text-sm font-semibold">
              Multi-need task sequence
            </h2>
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Session</TableHead>
                  <TableHead>Step</TableHead>
                  <TableHead>Transport</TableHead>
                  <TableHead>Request</TableHead>
                  <TableHead>Relation</TableHead>
                  <TableHead>Obligations</TableHead>
                  <TableHead>Worker avoided</TableHead>
                  <TableHead>Delivery</TableHead>
                  <TableHead>Main tools</TableHead>
                  <TableHead>Usage</TableHead>
                  <TableHead>Cost</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {(control.data?.semantic.need_steps ?? []).map(
                  ({
                    session_id,
                    step,
                    request,
                    main_turn_observations,
                    cost_microcredits,
                  }) => (
                    <TableRow key={step.id}>
                      <TableCell className="max-w-52 truncate font-mono">
                        {session_id}
                      </TableCell>
                      <TableCell>{step.ordinal}</TableCell>
                      <TableCell className="font-mono text-xs">
                        {request
                          ? `${request.transport ?? "legacy"}/${request.request_format ?? "unknown"}`
                          : "legacy/unknown"}
                      </TableCell>
                      <TableCell
                        className="max-w-64 truncate font-mono text-xs"
                        title={request?.raw_message}
                      >
                        {request
                          ? request.raw_message.replace(/\s+/g, " ")
                          : "Legacy record unavailable"}
                      </TableCell>
                      <TableCell>{step.relation}</TableCell>
                      <TableCell>
                        {step.satisfied.length} satisfied / {step.missing.length} missing
                      </TableCell>
                      <TableCell>{step.worker_avoided ? "Yes" : "No"}</TableCell>
                      <TableCell>{step.delivery ?? step.state}</TableCell>
                      <TableCell>
                        {step.main_discovery_tainted ? "Tainted" : "Clean"}
                      </TableCell>
                      <TableCell className="font-mono">
                        {latestUsage(main_turn_observations)}
                      </TableCell>
                      <TableCell className="font-mono">
                        {cost_microcredits == null
                          ? "Not priced"
                          : `${cost_microcredits} μcr`}
                      </TableCell>
                    </TableRow>
                  )
                )}
              </TableBody>
            </Table>
          </section>
          <section className="border bg-panel">
            <h2 className="border-b px-4 py-3 text-sm font-semibold">
              Verified change workflow
            </h2>
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Change</TableHead>
                  <TableHead>State</TableHead>
                  <TableHead>Revision</TableHead>
                  <TableHead>Patcher / verifier</TableHead>
                  <TableHead>Verification</TableHead>
                  <TableHead>Apply</TableHead>
                  <TableHead>Usage</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {(control.data?.changes ?? []).map((change) => (
                  <TableRow key={change.change_id}>
                    <TableCell className="font-mono">{change.change_id}</TableCell>
                    <TableCell>{change.state}</TableCell>
                    <TableCell>{change.revision}</TableCell>
                    <TableCell>
                      {change.attempts.filter((attempt) => attempt.role === "patcher").length} /{" "}
                      {change.attempts.filter((attempt) => attempt.role === "verifier").length}
                    </TableCell>
                    <TableCell>{change.verification?.verdict ?? "not_requested"}</TableCell>
                    <TableCell>
                      {change.applies.at(-1)?.status ?? "not_applied"}
                    </TableCell>
                    <TableCell className="font-mono">
                      {change.attempts.reduce(
                        (total, attempt) =>
                          total +
                          (attempt.usage.input_tokens ?? 0) +
                          (attempt.usage.output_tokens ?? 0),
                        0
                      )} tokens
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </section>
        </div>
      ) : resource === "settings" ? (
        <div className="grid gap-4 lg:grid-cols-2">
          <SettingsEditor
            key={control.data?.settings_digest ?? "settings"}
            control={control.data ?? null}
          />
          <SettingsPanel
            title="Worker"
            rows={[
              ["Transport", control.data?.runtime.transport ?? "—"],
              ["Sandbox", control.data?.runtime.sandbox ?? "—"],
              ["Approval policy", control.data?.runtime.approval_policy ?? "—"],
              [
                "Model",
                control.data?.settings?.worker_model ?? "Not configured",
              ],
              [
                "Reasoning",
                control.data?.settings?.worker_reasoning ?? "Not configured",
              ],
              [
                "Trusted tests",
                control.data?.settings?.trusted_test_execution
                  ? "Enabled"
                  : "Disabled",
              ],
            ]}
          />
          <SettingsPanel
            title="Security & storage"
            rows={[
              ["Storage", "SQLite"],
              ["Network", "Disabled for workers"],
              ["External telemetry", "Disabled"],
              ["File changes", "Always declined"],
              ["Definition updates", "If-Match required"],
            ]}
          />
          <SettingsPanel
            title="Multi-need coordination"
            rows={[
              [
                "Multi-need",
                control.data?.settings?.multi_need_enabled ? "Enabled" : "Disabled",
              ],
              [
                "Continue working",
                control.data?.settings?.continue_working_enabled
                  ? "Enabled"
                  : "Disabled",
              ],
              [
                "Task bounds",
                control.data?.settings
                  ? `${control.data.settings.max_needs_per_task} needs / ${control.data.settings.max_workers_per_task} workers`
                  : "Not configured",
              ],
              ["Resolver concurrency", "1"],
            ]}
          />
        </div>
      ) : (
        <section className="border bg-panel">
          <Empty className="min-h-96 rounded-none border-0">
            <EmptyHeader>
              <Icon />
              <EmptyTitle>No recorded data</EmptyTitle>
              <EmptyDescription>
                The Phase 0–4 configuration surface is available. Data appears
                only after a validated local run or approved experiment.
              </EmptyDescription>
            </EmptyHeader>
          </Empty>
        </section>
      )}
    </div>
  )
}

function RecordedDataEmpty({ icon: Icon }: { icon: typeof Boxes }) {
  return (
    <Empty className="min-h-52 rounded-none border-0">
      <EmptyHeader>
        <Icon />
        <EmptyTitle>No recorded data</EmptyTitle>
        <EmptyDescription>
          Records appear only after a validated local run.
        </EmptyDescription>
      </EmptyHeader>
    </Empty>
  )
}

function MetricPanel({ label, value }: { label: string; value: number }) {
  return (
    <section className="border bg-panel p-4">
      <p className="text-xs tracking-wide text-muted-foreground uppercase">
        {label}
      </p>
      <p className="mt-2 font-mono text-3xl font-semibold">{value}</p>
    </section>
  )
}

function latestUsage(
  observations: ControlPlane["semantic"]["need_steps"][number]["main_turn_observations"]
) {
  const latest = observations.at(-1)
  if (!latest) return "—"
  try {
    const usage = JSON.parse(latest.usage_json) as {
      input_tokens?: number | null
      cached_input_tokens?: number | null
      output_tokens?: number | null
    }
    return `${usage.input_tokens ?? "?"}/${usage.cached_input_tokens ?? "?"}/${usage.output_tokens ?? "?"}`
  } catch {
    return "Unavailable"
  }
}

function SettingsEditor({ control }: { control: ControlPlane | null }) {
  const update = useSettingsUpdate()
  const [draft, setDraft] = useState<SettingsConfig | null>(
    control?.settings ?? null
  )

  if (!draft) {
    return (
      <section className="border bg-panel p-4 text-sm text-muted-foreground">
        Runtime settings are not configured.
      </section>
    )
  }

  return (
    <form
      className="border bg-panel"
      onSubmit={(event) => {
        event.preventDefault()
        if (control?.settings_digest) {
          update.mutate({ settings: draft, digest: control.settings_digest })
        }
      }}
    >
      <div className="flex items-center justify-between border-b px-4 py-3">
        <h2 className="text-sm font-semibold">Worker configuration</h2>
        <Badge variant="outline">If-Match protected</Badge>
      </div>
      <div className="grid gap-4 p-4">
        <p className="text-xs text-muted-foreground">
          Model order and reasoning are configured on the Models page.
        </p>
        <label className="grid gap-1.5 text-sm">
          <span className="text-muted-foreground">Timeout seconds</span>
          <Input
            type="number"
            min={1}
            max={3600}
            value={draft.worker_timeout_seconds}
            onChange={(event) =>
              setDraft({
                ...draft,
                worker_timeout_seconds: Number(event.target.value),
              })
            }
          />
        </label>
        <label className="grid gap-1.5 text-sm">
          <span className="text-muted-foreground">Evidence failure policy</span>
          <select
            className="h-9 border bg-background px-3 text-sm"
            value={draft.evidence_failure_policy}
            onChange={(event) =>
              setDraft({
                ...draft,
                evidence_failure_policy: event.target
                  .value as SettingsConfig["evidence_failure_policy"],
              })
            }
          >
            <option value="discard_invalid_fact">Discard invalid fact</option>
            <option value="repair_once">Repair once</option>
          </select>
        </label>
        <label className="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={draft.trusted_test_execution}
            onChange={(event) =>
              setDraft({
                ...draft,
                trusted_test_execution: event.target.checked,
              })
            }
          />
          Trust this repository profile for policy-matched test execution
        </label>
        <div className="grid gap-3 border-t pt-4">
          <p className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
            Multi-need coordination
          </p>
          <label className="flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              checked={draft.multi_need_enabled}
              onChange={(event) =>
                setDraft({ ...draft, multi_need_enabled: event.target.checked })
              }
            />
            Allow multiple bounded needs in one task
          </label>
          <label className="flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              checked={draft.continue_working_enabled}
              onChange={(event) =>
                setDraft({
                  ...draft,
                  continue_working_enabled: event.target.checked,
                })
              }
            />
            Allow explicit continue-working delivery
          </label>
          <div className="grid grid-cols-2 gap-3">
            <label className="grid gap-1.5 text-sm">
              <span className="text-muted-foreground">Needs per task</span>
              <Input
                type="number"
                min={1}
                max={8}
                value={draft.max_needs_per_task}
                onChange={(event) =>
                  setDraft({
                    ...draft,
                    max_needs_per_task: Number(event.target.value),
                  })
                }
              />
            </label>
            <label className="grid gap-1.5 text-sm">
              <span className="text-muted-foreground">Workers per task</span>
              <Input
                type="number"
                min={1}
                max={8}
                value={draft.max_workers_per_task}
                onChange={(event) =>
                  setDraft({
                    ...draft,
                    max_workers_per_task: Number(event.target.value),
                  })
                }
              />
            </label>
          </div>
        </div>
        {update.error ? (
          <p className="text-sm text-destructive">{update.error.message}</p>
        ) : null}
        <Button
          className="justify-self-start"
          type="submit"
          disabled={update.isPending || !control?.settings_digest}
        >
          {update.isPending ? "Saving..." : "Save settings"}
        </Button>
      </div>
    </form>
  )
}

function ModelPolicyEditor({ control }: { control: ControlPlane | null }) {
  const update = useModelPolicyUpdate()
  const fixed =
    control?.model_policy && "fixed_order" in control.model_policy
      ? control.model_policy.fixed_order
      : null
  const cheapest =
    control?.model_policy && "cheapest_validated_first" in control.model_policy
      ? control.model_policy.cheapest_validated_first
      : null
  const currentProfiles = fixed?.profiles ?? cheapest?.promoted_profiles ?? []
  const [mode, setMode] = useState<"fixed_order" | "cheapest_validated_first">(
    fixed || !cheapest ? "fixed_order" : "cheapest_validated_first"
  )
  const [profiles, setProfiles] = useState(
    currentProfiles
      .map(
        (profile) =>
          `${profile.model} | ${profile.reasoning} | ${profile.service_tier ?? ""}`
      )
      .join("\n")
  )
  const [repairOnce, setRepairOnce] = useState(fixed?.repair_once ?? false)
  const [nativeFallback, setNativeFallback] = useState(
    fixed?.native_fallback ?? cheapest?.native_fallback ?? true
  )
  const [parseError, setParseError] = useState<string | null>(null)

  return (
    <form
      className="border bg-panel"
      onSubmit={(event) => {
        event.preventDefault()
        const parsed = profiles
          .split("\n")
          .map((line) => line.split("|").map((part) => part.trim()))
          .filter((parts) => parts.some(Boolean))
          .map(([model = "", reasoning = "", serviceTier = ""]) => ({
            model,
            reasoning,
            service_tier: serviceTier || null,
          }))
        if (
          parsed.length === 0 ||
          parsed.some((profile) => !profile.model || !profile.reasoning)
        ) {
          setParseError("Each profile needs model and reasoning.")
          return
        }
        setParseError(null)
        const policy: ModelPolicyInput =
          mode === "fixed_order"
            ? {
                fixed_order: {
                  profiles: parsed,
                  repair_once: repairOnce,
                  native_fallback: nativeFallback,
                },
              }
            : {
                cheapest_validated_first: {
                  profiles: parsed,
                  native_fallback: nativeFallback,
                },
              }
        if (control?.model_policy_digest) {
          update.mutate({ policy, digest: control.model_policy_digest })
        }
      }}
    >
      <div className="flex items-center justify-between border-b px-4 py-3">
        <h2 className="text-sm font-semibold">Model ladder configuration</h2>
        <Badge variant="outline">Immutable profile digests</Badge>
      </div>
      <div className="grid gap-4 p-4">
        <label className="grid gap-1.5 text-sm">
          <span className="text-muted-foreground">Policy</span>
          <select
            className="h-9 border bg-background px-3 text-sm"
            value={mode}
            onChange={(event) =>
              setMode(
                event.target.value as "fixed_order" | "cheapest_validated_first"
              )
            }
          >
            <option value="fixed_order">Fixed order</option>
            <option value="cheapest_validated_first">
              Cheapest validated first
            </option>
          </select>
        </label>
        <label className="grid gap-1.5 text-sm">
          <span className="text-muted-foreground">
            Profiles, one per line: model | reasoning | service tier
          </span>
          <textarea
            className="min-h-28 border bg-background px-3 py-2 font-mono text-sm"
            value={profiles}
            onChange={(event) => setProfiles(event.target.value)}
          />
        </label>
        {mode === "fixed_order" ? (
          <label className="flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              checked={repairOnce}
              onChange={(event) => setRepairOnce(event.target.checked)}
            />
            One repair turn before escalation
          </label>
        ) : null}
        <label className="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={nativeFallback}
            onChange={(event) => setNativeFallback(event.target.checked)}
          />
          Native fallback after ladder exhaustion
        </label>
        {mode === "cheapest_validated_first" ? (
          <p className="text-xs text-muted-foreground">
            Activation is rejected unless every enabled route/profile pair is
            already promoted.
          </p>
        ) : null}
        {parseError ? (
          <p className="text-sm text-destructive">{parseError}</p>
        ) : null}
        {update.error ? (
          <p className="text-sm text-destructive">{update.error.message}</p>
        ) : null}
        <Button
          className="justify-self-start"
          type="submit"
          disabled={update.isPending || !control?.model_policy_digest}
        >
          {update.isPending ? "Saving..." : "Save model policy"}
        </Button>
      </div>
    </form>
  )
}

function ModelPolicyPanel({ control }: { control: ControlPlane | null }) {
  const fixed =
    control?.model_policy && "fixed_order" in control.model_policy
      ? control.model_policy.fixed_order
      : null
  const cheapest =
    control?.model_policy && "cheapest_validated_first" in control.model_policy
      ? control.model_policy.cheapest_validated_first
      : null
  const profiles = fixed?.profiles ?? cheapest?.promoted_profiles ?? []

  return (
    <div className="grid gap-4">
      <ModelPolicyEditor control={control} />
      <section className="border bg-panel">
        <div className="flex items-center justify-between border-b px-4 py-3">
          <h2 className="text-sm font-semibold">
            {fixed ? "Fixed order" : "Cheapest validated first"}
          </h2>
          <Badge variant="outline">
            {(fixed?.native_fallback ?? cheapest?.native_fallback)
              ? "Native fallback"
              : "No fallback"}
          </Badge>
        </div>
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Order</TableHead>
              <TableHead>Model</TableHead>
              <TableHead>Reasoning</TableHead>
              <TableHead>Service tier</TableHead>
              <TableHead>Profile digest</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {profiles.map((profile, index) => (
              <TableRow key={profile.definition_digest}>
                <TableCell className="font-mono">{index + 1}</TableCell>
                <TableCell>{profile.model}</TableCell>
                <TableCell>{profile.reasoning}</TableCell>
                <TableCell>{profile.service_tier ?? "Default"}</TableCell>
                <TableCell className="max-w-72 truncate font-mono">
                  {profile.definition_digest}
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
        {profiles.length === 0 ? (
          <Empty className="min-h-52 rounded-none border-0">
            <EmptyHeader>
              <SlidersHorizontal />
              <EmptyTitle>No model policy configured</EmptyTitle>
            </EmptyHeader>
          </Empty>
        ) : null}
      </section>
    </div>
  )
}

function SettingsPanel({
  title,
  rows,
}: {
  title: string
  rows: Array<[string, string]>
}) {
  return (
    <section className="border bg-panel">
      <h2 className="border-b px-4 py-3 text-sm font-semibold">{title}</h2>
      <dl className="divide-y">
        {rows.map(([label, value]) => (
          <div
            key={label}
            className="grid grid-cols-[9rem_1fr] gap-4 px-4 py-3 text-sm"
          >
            <dt className="text-muted-foreground">{label}</dt>
            <dd className="font-mono">{value}</dd>
          </div>
        ))}
      </dl>
    </section>
  )
}
