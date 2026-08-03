import { useEffect, useState, type ReactNode } from "react"

import {
  type RoleProfileDefinitionInput,
  RoleProfileApiError,
  type RoleProfileDefinition,
  type RoleProfileRole,
  type RoleProfileState,
  type RoleProfilePreflightResponse,
  useRoleProfile,
  useRoleProfileActivate,
  useRoleProfileAudit,
  useRoleProfileDeactivate,
  useRoleProfileDraft,
  useRoleProfilePreflight,
  useRoleProfileRevisions,
  useRoleProfiles,
} from "@/api"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"

const EMPTY_DIGEST = `b3:${"0".repeat(64)}`

const roles: RoleProfileRole[] = [
  "explorer",
  "implementer",
  "test_runner",
  "reviewer",
  "verifier",
  "auditor",
]

function emptyDraft(profileId = "explorer.default"): RoleProfileDefinitionInput {
  return {
    profile_id: profileId,
    role: "explorer",
    host: "codex",
    model: "gpt-5",
    reasoning: "medium",
    service_tier: "default",
    timeout_seconds: 180,
    budget: { max_turns: 2, max_output_tokens: 1200, max_cost_microusd: 1000 },
    prompt_profile_digest: EMPTY_DIGEST,
    output_contract_digest: EMPTY_DIGEST,
    tool_policy: "read_only",
    command_policy: "read_only",
    filesystem_policy: "read_only_checkout",
    network_policy: "denied",
    test_policy: "disabled",
    repair_policy: "none",
    fallback_policy: "native",
    concurrency: 1,
    route_assignments: [],
  }
}

function inputFromDefinition(
  definition: RoleProfileDefinitionInput & { definition_digest?: string },
): RoleProfileDefinitionInput {
  const input = { ...definition }
  delete input.definition_digest
  return {
    ...input,
    budget: { ...input.budget },
    route_assignments: [...input.route_assignments],
  }
}

function apiErrorMessage(error: unknown) {
  if (error instanceof RoleProfileApiError && (error.status === 409 || error.status === 412)) {
    return `The profile changed on the server; current data was refreshed. (${error.message})`
  }
  if (error && typeof error === "object" && "message" in error) {
    return String((error as { message: unknown }).message)
  }
  return "The role-profile request failed."
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <label className="grid gap-1 text-xs text-muted-foreground">
      <span>{label}</span>
      {children}
    </label>
  )
}

function StateBadge({ state }: { state: RoleProfileState }) {
  return <Badge variant={state === "active" ? "default" : "outline"}>{state}</Badge>
}

type ActivationConfirmation = {
  profileId: string
  revision: number
  definitionDigest: string
  stateDigest: string
}

type DeactivationConfirmation = {
  profileId: string
  revision: number
  activeDefinitionDigest: string
  stateDigest: string
}

function sameActivationConfirmation(
  confirmation: ActivationConfirmation | null,
  target: ActivationConfirmation | null,
) {
  return Boolean(
    confirmation &&
      target &&
      confirmation.profileId === target.profileId &&
      confirmation.revision === target.revision &&
      confirmation.definitionDigest === target.definitionDigest &&
      confirmation.stateDigest === target.stateDigest,
  )
}

function sameDeactivationConfirmation(
  confirmation: DeactivationConfirmation | null,
  target: DeactivationConfirmation | null,
) {
  return Boolean(
    confirmation &&
      target &&
      confirmation.profileId === target.profileId &&
      confirmation.revision === target.revision &&
      confirmation.activeDefinitionDigest === target.activeDefinitionDigest &&
      confirmation.stateDigest === target.stateDigest,
  )
}

export default function RoleProfilesEditor() {
  const profiles = useRoleProfiles()
  const [selectedId, setSelectedId] = useState<string | null | undefined>(undefined)
  const [selectedRevision, setSelectedRevisionState] = useState<number | undefined>()
  const [draft, setDraft] = useState<RoleProfileDefinitionInput>(emptyDraft())
  const [preflightResult, setPreflightResult] = useState<RoleProfilePreflightResponse | null>(null)
  const [activationConfirmation, setActivationConfirmation] = useState<ActivationConfirmation | null>(null)
  const [deactivationConfirmation, setDeactivationConfirmation] = useState<DeactivationConfirmation | null>(null)
  const [notice, setNotice] = useState<string | null>(null)

  const setSelectedRevision = (revision: number | undefined) => {
    setSelectedRevisionState(revision)
    setActivationConfirmation(null)
    setDeactivationConfirmation(null)
  }

  const detail = useRoleProfile(selectedId ?? undefined, selectedRevision)
  const revisions = useRoleProfileRevisions(selectedId ?? undefined)
  const audit = useRoleProfileAudit(selectedId ?? undefined)
  const preflight = useRoleProfilePreflight()
  const save = useRoleProfileDraft()
  const activate = useRoleProfileActivate()
  const deactivate = useRoleProfileDeactivate()

  const activationTarget =
    selectedId != null && detail.data?.profile.profile_id === selectedId
      ? {
          profileId: detail.data.profile.profile_id,
          revision: selectedRevision ?? detail.data.profile.revision,
          definitionDigest: detail.data.profile.definition_digest,
          stateDigest: detail.data.profile.state_digest,
        }
      : null
  const deactivationTarget =
    selectedId != null &&
    detail.data?.profile.profile_id === selectedId &&
    detail.data.profile.active_definition_digest
      ? {
          profileId: detail.data.profile.profile_id,
          revision: selectedRevision ?? detail.data.profile.revision,
          activeDefinitionDigest: detail.data.profile.active_definition_digest,
          stateDigest: detail.data.profile.state_digest,
        }
      : null
  const activationConfirmed = sameActivationConfirmation(activationConfirmation, activationTarget)
  const deactivationConfirmed = sameDeactivationConfirmation(deactivationConfirmation, deactivationTarget)

  const startNewProfile = () => {
    setSelectedId(null)
    setDraft(emptyDraft())
    setSelectedRevision(undefined)
    setActivationConfirmation(null)
    setDeactivationConfirmation(null)
    setPreflightResult(null)
    setNotice(null)
  }

  useEffect(() => {
    if (selectedId === undefined && profiles.data?.items[0]) {
      // The query result is an external source; initialize the local selection once.
      // eslint-disable-next-line react-hooks/set-state-in-effect
      setSelectedId(profiles.data.items[0].profile_id)
    }
  }, [profiles.data?.items, selectedId])

  useEffect(() => {
    if (detail.data?.profile.definition) {
      // Keep the form aligned with an explicitly selected revision.
      // eslint-disable-next-line react-hooks/set-state-in-effect
      setDraft(inputFromDefinition(detail.data.profile.definition))
      setPreflightResult(null)
    }
  }, [detail.data?.profile.definition])

  const setField = <K extends keyof RoleProfileDefinitionInput>(
    field: K,
    value: RoleProfileDefinitionInput[K],
  ) => {
    setDraft((current) => ({ ...current, [field]: value }))
    setPreflightResult(null)
  }

  const setBudget = (field: keyof RoleProfileDefinitionInput["budget"], value: number) => {
    setDraft((current) => ({ ...current, budget: { ...current.budget, [field]: value } }))
    setPreflightResult(null)
  }

  const runPreflight = async (
    input: RoleProfileDefinitionInput = draft,
  ): Promise<RoleProfilePreflightResponse | null> => {
    setNotice(null)
    try {
      const result = await preflight.mutateAsync({
        profileId: input.profile_id,
        input,
      })
      setPreflightResult(result)
      return result
    } catch (error) {
      if (error instanceof RoleProfileApiError && error.body && typeof error.body === "object") {
        const body = error.body as Partial<RoleProfilePreflightResponse>
        if (Array.isArray(body.failures)) {
          setPreflightResult({
            schema: body.schema ?? "needle.role-profiles/1",
            operation: body.operation ?? "invalid",
            passed: false,
            failures: body.failures,
            if_match: body.if_match,
            definition: (body.definition as RoleProfileDefinition | null) ?? null,
            definition_digest: body.definition_digest ?? null,
            worker_profile: body.worker_profile ?? null,
            worker_profile_digest: body.worker_profile_digest ?? null,
          })
        }
      }
      setNotice(apiErrorMessage(error))
      return null
    }
  }

  const saveDraft = async () => {
    if (selectedId === undefined) {
      setNotice("Select or start a profile before saving its draft.")
      return
    }
    const loadedStateDigest =
      selectedId !== null && detail.data?.profile.profile_id === selectedId
        ? detail.data.profile.state_digest
        : null
    if (selectedId !== null && loadedStateDigest == null) {
      setNotice("Load the selected profile before saving its draft.")
      return
    }
    const result = await runPreflight()
    if (!result || !result.passed || !result.if_match) return
    if (loadedStateDigest != null && result.if_match !== loadedStateDigest) {
      await Promise.all([profiles.refetch(), detail.refetch(), revisions.refetch(), audit.refetch()])
      setNotice("The profile changed while preflighting; current data was refreshed without merging the draft.")
      return
    }
    try {
      await save.mutateAsync({
        profileId: draft.profile_id,
        input: draft,
        stateDigest: loadedStateDigest ?? result.if_match,
      })
      setSelectedId(draft.profile_id)
      setSelectedRevision(undefined)
      setNotice("Draft saved. Execution remains unbound until activation.")
    } catch (error) {
      setNotice(apiErrorMessage(error))
    }
  }

  const activateRevision = async () => {
    if (!detail.data || !activationConfirmed || !activationTarget) return
    const profile = detail.data.profile
    const result = await runPreflight(inputFromDefinition(profile.definition))
    if (!result || !result.passed || result.definition_digest !== profile.definition_digest) {
      setNotice("Activation requires a successful preflight for the exact revision digest.")
      return
    }
    try {
      await activate.mutateAsync({
        profileId: profile.profile_id,
        revision: profile.revision,
        definitionDigest: profile.definition_digest,
        stateDigest: profile.state_digest,
      })
      setActivationConfirmation(null)
      setNotice("Revision activated. No worker/session binding is performed by this editor.")
    } catch (error) {
      setNotice(apiErrorMessage(error))
    }
  }

  const deactivateProfile = async () => {
    if (!detail.data || !deactivationConfirmed || !deactivationTarget) return
    try {
      await deactivate.mutateAsync({
        profileId: detail.data.profile.profile_id,
        activeDefinitionDigest: deactivationTarget.activeDefinitionDigest,
        stateDigest: detail.data.profile.state_digest,
      })
      setDeactivationConfirmation(null)
      setNotice("Profile deactivated.")
    } catch (error) {
      setNotice(apiErrorMessage(error))
    }
  }

  if (profiles.isPending) return <div className="p-6 text-sm text-muted-foreground">Loading role profiles…</div>
  if (profiles.isError) return <div className="p-6 text-sm text-destructive">Unable to load role profiles: {apiErrorMessage(profiles.error)}</div>

  const profileItems = profiles.data?.items ?? []

  return (
    <div className="grid gap-5 p-4 md:p-6 xl:grid-cols-[minmax(15rem,0.28fr)_minmax(0,1fr)]">
      <aside className="border bg-panel">
        <div className="border-b px-4 py-3">
          <h1 className="text-lg font-semibold">Role profiles</h1>
          <p className="mt-1 text-xs text-muted-foreground">
            Canonical Codex configuration only; no worker or session binding.
          </p>
        </div>
        <div className="p-3">
          {profileItems.length === 0 ? (
            <div className="grid gap-2 py-6 text-sm text-muted-foreground">
              <span>No profiles yet.</span>
              <Button size="sm" variant="outline" onClick={startNewProfile}>
                Start a profile
              </Button>
            </div>
          ) : (
            <div className="grid gap-1">
              {profileItems.map((item) => (
                <button
                  type="button"
                  key={item.profile_id}
                  className={`grid gap-1 border px-3 py-2 text-left text-sm ${selectedId === item.profile_id ? "border-primary bg-accent" : "hover:bg-accent/50"}`}
                  onClick={() => { setSelectedId(item.profile_id); setSelectedRevision(undefined); setActivationConfirmation(null); setDeactivationConfirmation(null) }}
                >
                  <span className="flex items-center justify-between gap-2 font-mono">
                    {item.profile_id}
                    <StateBadge state={item.state} />
                  </span>
                  <span className="text-xs text-muted-foreground">{item.role} · revision {item.latest_revision}</span>
                </button>
              ))}
              <Button size="sm" variant="outline" className="mt-2" onClick={startNewProfile}>
                New profile
              </Button>
            </div>
          )}
        </div>
        <div className="border-t px-4 py-3 text-xs text-muted-foreground">
          Codex host is fixed and read-only. Non-Codex hosts are unavailable.
        </div>
      </aside>

      <main className="grid gap-5">
        {selectedId && detail.isPending ? <div className="text-sm text-muted-foreground">Loading revision…</div> : null}
        {selectedId && detail.isError ? <div className="text-sm text-destructive">Unable to load profile: {apiErrorMessage(detail.error)}</div> : null}
        <section className="border bg-panel">
          <div className="flex flex-wrap items-center justify-between gap-3 border-b px-4 py-3">
            <div>
              <h2 className="font-semibold">{selectedId ?? "New canonical profile"}</h2>
              <p className="text-xs text-muted-foreground">Single model profile; ModelPolicy owns the model ladder.</p>
            </div>
            {detail.data ? <StateBadge state={detail.data.profile.state} /> : null}
          </div>
          <div className="grid gap-4 p-4 md:grid-cols-2 xl:grid-cols-3">
            <Field label="Profile id"><Input value={draft.profile_id} readOnly={Boolean(selectedId)} onChange={(event) => setField("profile_id", event.target.value)} /></Field>
            <Field label="Role"><select className="h-9 border bg-background px-2 text-sm" value={draft.role} onChange={(event) => setField("role", event.target.value as RoleProfileRole)}>{roles.map((role) => <option key={role} value={role}>{role}</option>)}</select></Field>
            <Field label="Host (read-only Codex)"><Input value={draft.host} readOnly disabled /></Field>
            <Field label="Model"><Input value={draft.model} onChange={(event) => setField("model", event.target.value)} /></Field>
            <Field label="Reasoning"><select className="h-9 border bg-background px-2 text-sm" value={draft.reasoning} onChange={(event) => setField("reasoning", event.target.value as RoleProfileDefinitionInput["reasoning"])}>{["low", "medium", "high", "xhigh"].map((value) => <option key={value}>{value}</option>)}</select></Field>
            <Field label="Service tier"><select className="h-9 border bg-background px-2 text-sm" value={draft.service_tier} onChange={(event) => setField("service_tier", event.target.value as RoleProfileDefinitionInput["service_tier"])}><option value="default">default</option><option value="priority">priority</option></select></Field>
            <Field label="Timeout seconds"><Input type="number" min={1} max={3600} value={draft.timeout_seconds} onChange={(event) => setField("timeout_seconds", Number(event.target.value))} /></Field>
            <Field label="Max turns"><Input type="number" min={1} max={8} value={draft.budget.max_turns} onChange={(event) => setBudget("max_turns", Number(event.target.value))} /></Field>
            <Field label="Max output tokens"><Input type="number" min={1} value={draft.budget.max_output_tokens} onChange={(event) => setBudget("max_output_tokens", Number(event.target.value))} /></Field>
            <Field label="Max cost (microUSD)"><Input type="number" min={1} value={draft.budget.max_cost_microusd} onChange={(event) => setBudget("max_cost_microusd", Number(event.target.value))} /></Field>
            <Field label="Prompt profile digest"><Input value={draft.prompt_profile_digest} onChange={(event) => setField("prompt_profile_digest", event.target.value)} /></Field>
            <Field label="Output contract digest"><Input value={draft.output_contract_digest} onChange={(event) => setField("output_contract_digest", event.target.value)} /></Field>
            <Field label="Tool policy"><select className="h-9 border bg-background px-2 text-sm" value={draft.tool_policy} onChange={(event) => setField("tool_policy", event.target.value as RoleProfileDefinitionInput["tool_policy"])}><option value="read_only">read_only</option><option value="isolated_write">isolated_write</option></select></Field>
            <Field label="Command policy"><select className="h-9 border bg-background px-2 text-sm" value={draft.command_policy} onChange={(event) => setField("command_policy", event.target.value as RoleProfileDefinitionInput["command_policy"])}><option value="denied">denied</option><option value="read_only">read_only</option><option value="certified_tests">certified_tests</option></select></Field>
            <Field label="Filesystem policy"><select className="h-9 border bg-background px-2 text-sm" value={draft.filesystem_policy} onChange={(event) => setField("filesystem_policy", event.target.value as RoleProfileDefinitionInput["filesystem_policy"])}><option value="read_only_checkout">read_only_checkout</option><option value="disposable_checkout">disposable_checkout</option></select></Field>
            <Field label="Network policy (fixed denied)"><Input value={draft.network_policy} readOnly disabled /></Field>
            <Field label="Test policy"><select className="h-9 border bg-background px-2 text-sm" value={draft.test_policy} onChange={(event) => setField("test_policy", event.target.value as RoleProfileDefinitionInput["test_policy"])}><option value="disabled">disabled</option><option value="certified">certified</option></select></Field>
            <Field label="Repair policy"><select className="h-9 border bg-background px-2 text-sm" value={draft.repair_policy} onChange={(event) => setField("repair_policy", event.target.value as RoleProfileDefinitionInput["repair_policy"])}><option value="none">none</option><option value="once">once</option></select></Field>
            <Field label="Fallback policy"><select className="h-9 border bg-background px-2 text-sm" value={draft.fallback_policy} onChange={(event) => setField("fallback_policy", event.target.value as RoleProfileDefinitionInput["fallback_policy"])}><option value="disabled">disabled</option><option value="native">native</option></select></Field>
            <Field label="Routes (comma separated)"><Input value={draft.route_assignments.join(", ")} onChange={(event) => setField("route_assignments", event.target.value.split(",").map((value) => value.trim()).filter(Boolean))} /></Field>
          </div>
          <div className="flex flex-wrap items-center gap-2 border-t px-4 py-3">
            <Button onClick={() => void runPreflight()} disabled={preflight.isPending}>Preflight</Button>
            <Button variant="outline" onClick={() => void saveDraft()} disabled={save.isPending || preflight.isPending}>Save draft</Button>
            {preflightResult ? <Badge variant={preflightResult.passed ? "default" : "destructive"}>{preflightResult.passed ? "Preflight passed" : "Preflight failed"}</Badge> : null}
            {notice ? <span className="text-xs text-muted-foreground">{notice}</span> : null}
          </div>
          {preflightResult?.failures.length ? <div className="border-t px-4 py-3 text-sm text-destructive">{preflightResult.failures.join("; ")}</div> : null}
        </section>

        {detail.data ? (
          <section className="grid gap-4 border bg-panel p-4 lg:grid-cols-2">
            <div className="grid gap-2 text-xs">
              <h3 className="text-sm font-semibold">Revision and activation</h3>
              <div>Definition digest: <code>{detail.data.profile.definition_digest}</code></div>
              <div>Worker compatibility digest: <code>{detail.data.profile.worker_profile_digest ?? "—"}</code></div>
              <div>Request-time preflight: <Badge variant={detail.data.profile.preflight.passed ? "default" : "destructive"}>{detail.data.profile.preflight.passed ? "passed" : "failed"}</Badge></div>
              {detail.data.profile.preflight.failures.length ? <div className="text-destructive">{detail.data.profile.preflight.failures.join("; ")}</div> : null}
              <div>State digest / If-Match: <code>{detail.data.profile.state_digest}</code></div>
              <label className="flex items-center gap-2"><input type="checkbox" checked={activationConfirmed} onChange={(event) => setActivationConfirmation(event.target.checked && activationTarget ? activationTarget : null)} />Confirm exact digest activation</label>
              <Button size="sm" onClick={() => void activateRevision()} disabled={!activationConfirmed || activate.isPending}>Activate selected revision</Button>
              <label className="flex items-center gap-2"><input type="checkbox" checked={deactivationConfirmed} onChange={(event) => setDeactivationConfirmation(event.target.checked && deactivationTarget ? deactivationTarget : null)} />Confirm current active digest deactivation</label>
              <Button size="sm" variant="outline" onClick={() => void deactivateProfile()} disabled={!deactivationConfirmed || deactivate.isPending}>Deactivate active revision</Button>
            </div>
            <div className="grid gap-2 text-xs">
              <h3 className="text-sm font-semibold">History</h3>
              {revisions.isPending ? <span className="text-muted-foreground">Loading history…</span> : null}
              {revisions.data?.items.map((item) => <button type="button" key={item.revision} className={`flex items-center justify-between border px-2 py-1 text-left ${item.revision === detail.data?.profile.revision ? "border-primary" : ""}`} onClick={() => setSelectedRevision(item.revision)}><span>Revision {item.revision} · {item.model}</span><StateBadge state={item.state} /></button>)}
            </div>
          </section>
        ) : null}

        {selectedId ? (
          <section className="border bg-panel">
            <div className="border-b px-4 py-3 text-sm font-semibold">Audit (bounded)</div>
            <div className="divide-y text-xs">{audit.data?.items.map((item) => <div key={item.audit_id} className="grid gap-1 px-4 py-2 md:grid-cols-[auto_auto_1fr_auto]"><span>#{item.audit_id}</span><span>{item.operation}</span><code>{item.definition_digest}</code><span>{item.created_unix_ms}</span></div>) ?? <div className="px-4 py-4 text-muted-foreground">No audit records.</div>}</div>
          </section>
        ) : null}
      </main>
    </div>
  )
}
