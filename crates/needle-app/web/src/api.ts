import { useEffect } from "react"
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query"

export type ApprovalDecision = "accept" | "decline" | "cancel"

export interface ApprovalRequest {
  id: string
  thread_id: string
  turn_id: string
  item_id: string
  argv: string[]
  command_display: string | null
  cwd: string
  reason: string | null
  requested_permissions: {
    write_paths: string[]
    read_paths: string[]
    network: boolean
    raw: unknown
  }
  route: string
  repository_id: string
  repository_root: string
  expires_unix_ms: number
  classification:
    | "pending_user"
    | "rejected_file_change"
    | "rejected_network"
    | "rejected_unparseable"
    | "rejected_policy_mismatch"
    | "expired"
    | { auto_approved_test: { policy_id: string } }
    | { auto_approved_read_only: { policy_id: string } }
  payload_digest: string
  decision: ApprovalDecision | null
}

export interface RouteDefinition {
  id: string
  enabled: boolean
  priority: number
  preset_id: string
  definition_digest: string
}

export interface PlanNode {
  id: string
  operator_id: string
  depends_on: string[]
}

export interface RoutePlan {
  id: string
  revision: number
  definition_digest: string
  nodes: PlanNode[]
}

export interface SettingsConfig {
  worker_model: string
  worker_reasoning: string
  worker_timeout_seconds: number
  evidence_failure_policy: "discard_invalid_fact" | "repair_once"
  trusted_test_execution: boolean
  multi_need_enabled: boolean
  continue_working_enabled: boolean
  max_needs_per_task: number
  max_workers_per_task: number
  pending_main_tools: "allow_and_taint"
  resolver_concurrency: number
}

export type ModelPolicy =
  | {
      fixed_order: {
        profiles: WorkerProfile[]
        repair_once: boolean
        native_fallback: boolean
      }
    }
  | {
      cheapest_validated_first: {
        promoted_profiles: WorkerProfile[]
        native_fallback: boolean
      }
    }

export type ModelPolicyInput =
  | {
      fixed_order: {
        profiles: Array<{
          model: string
          reasoning: string
          service_tier: string | null
        }>
        repair_once: boolean
        native_fallback: boolean
      }
    }
  | {
      cheapest_validated_first: {
        profiles: Array<{
          model: string
          reasoning: string
          service_tier: string | null
        }>
        native_fallback: boolean
      }
    }

export interface ControlPlane {
  schema: string
  runtime: {
    status: string
    transport: string
    sandbox: string
    approval_policy: string
    storage: string
    external_telemetry: boolean
  }
  phases: Array<{ id: number; name: string; available: boolean }>
  routes: RouteDefinition[]
  plans: RoutePlan[]
  settings: SettingsConfig | null
  settings_digest: string | null
  model_policy: ModelPolicy | null
  model_policy_digest: string | null
  cache: Array<{
    identity_digest: string
    logical_digest: string
    source_digest: string
    created_unix_ms: number
    hit_count: number
  }>
  artifacts: Array<{
    id: string
    request_id: string
    contract_id: string
    kind: string
    scope: "snapshot_exact" | "worktree_semantic"
    dependency_count: number
    validation_count: number
    created_unix_ms: number
  }>
  worker_runs: number
  execution_attempts: number
  command_evidence: number
  pending_approvals: number
  route_promotions: Array<{
    route_key: string
    worker_profile_digest: string
    evidence_digest: string
    promoted_unix_ms: number
  }>
  changes: ChangeWorkflow[]
  semantic: {
    format_revision: number
    needs: SemanticNeed[]
    subjects: SemanticSubject[]
    capabilities: CapabilityClass[]
    predicate_contracts: PredicateContract[]
    route_contracts: RouteContractDefinition[]
    selected_plans: SelectedProofPlan[]
    proofs: SufficiencyProof[]
    proof_accounting: ProofAccountingRecord[]
    need_steps: NeedStepEnvelope[]
    metrics: {
      authoritative_full_reuse: number
      authoritative_partial_reuse: number
      worker_avoided: number
      proof_overhead_micros: number
      stale_candidates: number
      active_contradictions: number
    }
  }
  cost_observations: unknown[]
}

export type PatchOperation = "create" | "update" | "delete"
export type VerificationStatus =
  | "not_requested"
  | "verified"
  | "rejected"
  | "repairable"
  | "inconclusive"

export interface ChangedFile {
  path: string
  operation: PatchOperation
}

export interface ChangeAttempt {
  role: "patcher" | "verifier"
  patch_id: string
  attempt: Record<string, unknown>
  usage: {
    input_tokens?: number | null
    cached_input_tokens?: number | null
    output_tokens?: number | null
    duration_ms?: number | null
  }
  cost_microusd: number | null
  created_unix_ms: number
}

export interface VerificationArtifact {
  id: string
  change_id: string
  patch_id: string
  verdict: VerificationStatus
  acceptance_coverage: Array<{
    criterion: string
    status: "addressed" | "partial" | "unaddressed"
    evidence: string
  }>
  findings: string[]
  test_evidence_ids: string[]
  verifier_definition: string
  created_unix_ms: number
}

export interface ChangeApplyRecord {
  id: string
  change_id: string
  patch_id: string
  repository_root: string
  pre_snapshot: string
  post_snapshot: string | null
  status:
    | "applying"
    | "applied"
    | "rolled_back"
    | "rollback_failed"
    | "recovery_conflict"
  created_unix_ms: number
  completed_unix_ms: number | null
}

export interface ChangeWorkflow {
  change_id: string
  patch_id: string
  revision: number
  state: string
  attempts: ChangeAttempt[]
  verification: VerificationArtifact | null
  applies: ChangeApplyRecord[]
}

export interface ChangeListItem {
  change_id: string
  state: string
  patch_id: string
  revision: number
  summary: string
  changed_files: ChangedFile[]
  change_digest: string
  created_unix_ms: number
}

export interface ChangeDetail {
  change: {
    request: {
      task: string
      acceptance_criteria: string[]
      constraints: string[]
    }
    state: string
    patch: {
      id: string
      change_id: string
      revision: number
      summary: string
      files: ChangedFile[]
      residual_risks: string[]
    }
  }
  verification: VerificationArtifact | null
  attempts: ChangeAttempt[]
  applies: ChangeApplyRecord[]
  change_digest: string
  apply_allowed: boolean
}

export interface NeedStep {
  id: string
  ordinal: number
  turn_id: string
  need_id: string
  coordination: "wait-response" | "continue-working"
  relation:
    | "repeat"
    | "residual"
    | "extension"
    | "overlap"
    | "independent"
    | "incompatible"
  state:
    | "requested"
    | "queued"
    | "resolving"
    | "resolved"
    | "delivered"
    | "native_fallback"
    | "failed"
    | "cancelled"
  required: string[]
  satisfied: string[]
  missing: string[]
  artifacts: string[]
  proof: string | null
  delivery:
    | "turn_start"
    | "turn_steer"
    | "already_satisfied"
    | "native_fallback"
    | null
  worker_avoided: boolean
  main_discovery_tainted: boolean
}

export interface NeedStepEnvelope {
  session_id: string
  step: NeedStep
  request: NeedStepRequest | null
  main_turn_observations: Array<{
    turn_id: string
    status: string
    delivery: string | null
    usage_json: string
    tools_json: string
    main_discovery_tainted: boolean
  }>
  cost_microcredits: number | null
}

export interface NeedStepRequest {
  need_step_id: string
  session_id: string
  request_digest: string
  raw_message: string
  semantic_interrupt: unknown | null
  need_ir: unknown | null
  transport: string | null
  request_format: string | null
  created_unix_ms: number
}

export interface SemanticFacet {
  key: string
  value: string
}

export interface SemanticObligation {
  id: string
  predicate:
    | "implementation-location"
    | "runtime-flow"
    | "focused-tests"
  subject: string
  facets: SemanticFacet[]
}

export interface SemanticNeed {
  id: string
  required: SemanticObligation[]
  preferred: SemanticObligation[]
  residual: { reason: string; mandatory: boolean } | null
  world: {
    repository_lineage: string
    source_selector: string
    platform: string
    features: string
  }
  format_revision: number
}

export interface SemanticSubject {
  id: string
  kind: string
  canonical_name: string
  repository_lineage: string
}

export interface CapabilityClass {
  id: string
  predicate: string
  reuse_unit?: "artifact" | "claim"
  exact_subject_only: boolean
  positive_only: boolean
  single_world_only: boolean
  composition: boolean
  mode: "disabled" | "shadow" | "advisory" | "authoritative"
  definition_digest: string
}

export interface PredicateContract {
  predicate: string
  allowed_subject_kinds: string[]
  allowed_facets: string[]
  world_dimensions: string[]
  definition_digest: string
}

export interface RouteContractDefinition {
  route: string
  required: Array<{ predicate: string; facets: SemanticFacet[] }>
  preferred: Array<{ predicate: string; facets: SemanticFacet[] }>
  definition_digest: string
}

export interface SelectedProofPlan {
  id: string
  need: string
  artifact_ids: string[]
  claim_ids?: string[]
  claim_validation_certificate_ids?: string[]
  claim_set_certificate_ids?: string[]
  covered_mask: number
  missing_mask: number
  decision_reason: string
  economics: {
    expected_fresh_microusd: number | null
    expected_selected_microusd: number | null
    proof_overhead_micros: number
    expected_net_microusd: number | null
  }
}

export interface SufficiencyProof {
  id: string
  need: string
  obligations: string[]
  artifacts: string[]
  validation_certificates: string[]
  world_digest: string
  engine_definition: string
}

export interface ProofAccountingRecord {
  need_id: string
  plan_id: string | null
  parse_micros: number
  lookup_micros: number
  validation_micros: number
  planning_micros: number
  projection_micros: number
  allocation_count: number | null
  allocated_bytes: number | null
  stale_candidates: number
  created_unix_ms: number
}

export interface WorkerProfile {
  platform: string
  model: string
  reasoning: string
  service_tier: string | null
  definition_digest: string
}

async function getJson<T>(url: string): Promise<T> {
  const response = await fetch(url, { credentials: "same-origin" })
  if (!response.ok) {
    throw new Error(`Request failed with ${response.status}`)
  }
  return response.json() as Promise<T>
}

function csrfToken() {
  return (
    document
      .querySelector<HTMLMetaElement>('meta[name="needle-csrf"]')
      ?.getAttribute("content") ?? ""
  )
}

export function useControlPlane() {
  return useQuery({
    queryKey: ["control-plane"],
    queryFn: () => getJson<ControlPlane>("/api/v1/control-plane"),
  })
}

export function useApprovals() {
  return useQuery({
    queryKey: ["approvals"],
    queryFn: () =>
      getJson<ApprovalRequest[]>("/api/v1/approvals?status=pending"),
    refetchInterval: 1000,
  })
}

export function useApprovalEvents() {
  const queryClient = useQueryClient()
  useEffect(() => {
    const events = new EventSource("/api/v1/approvals/events")
    const refresh = () => {
      void Promise.all([
        queryClient.invalidateQueries({ queryKey: ["approvals"] }),
        queryClient.invalidateQueries({ queryKey: ["control-plane"] }),
      ])
    }
    events.addEventListener("approval-change", refresh)
    return () => {
      events.removeEventListener("approval-change", refresh)
      events.close()
    }
  }, [queryClient])
}

export function useApprovalDecision() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: async ({
      request,
      decision,
    }: {
      request: ApprovalRequest
      decision: ApprovalDecision
    }) => {
      const response = await fetch(
        `/api/v1/approvals/${encodeURIComponent(request.id)}/decision`,
        {
          method: "POST",
          credentials: "same-origin",
          headers: {
            "content-type": "application/json",
            "x-csrf-token": csrfToken(),
          },
          body: JSON.stringify({
            decision,
            payload_digest: request.payload_digest,
          }),
        }
      )
      if (!response.ok) {
        const body = (await response.json().catch(() => null)) as {
          error?: string
        } | null
        throw new Error(
          body?.error ?? `Decision failed with ${response.status}`
        )
      }
      return response.json() as Promise<ApprovalRequest>
    },
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["approvals"] }),
        queryClient.invalidateQueries({ queryKey: ["control-plane"] }),
      ])
    },
  })
}

export function useRouteState() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: async ({
      route,
      enabled,
    }: {
      route: RouteDefinition
      enabled: boolean
    }) => {
      const response = await fetch(
        `/api/v1/routes/${encodeURIComponent(route.id)}/state`,
        {
          method: "POST",
          credentials: "same-origin",
          headers: {
            "content-type": "application/json",
            "if-match": `"${route.definition_digest}"`,
            "x-csrf-token": csrfToken(),
          },
          body: JSON.stringify({ enabled }),
        }
      )
      if (!response.ok) {
        throw new Error(`Route update failed with ${response.status}`)
      }
      return response.json() as Promise<RouteDefinition>
    },
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["control-plane"] })
    },
  })
}

export function useSettingsUpdate() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: async ({
      settings,
      digest,
    }: {
      settings: SettingsConfig
      digest: string
    }) => {
      const response = await fetch("/api/v1/settings", {
        method: "POST",
        credentials: "same-origin",
        headers: {
          "content-type": "application/json",
          "if-match": `"${digest}"`,
          "x-csrf-token": csrfToken(),
        },
        body: JSON.stringify(settings),
      })
      if (!response.ok) {
        const body = (await response.json().catch(() => null)) as {
          error?: string
        } | null
        throw new Error(
          body?.error ?? `Settings update failed with ${response.status}`
        )
      }
      return response.json()
    },
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["control-plane"] })
    },
  })
}

export function useModelPolicyUpdate() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: async ({
      policy,
      digest,
    }: {
      policy: ModelPolicyInput
      digest: string
    }) => {
      const response = await fetch("/api/v1/model-policy", {
        method: "POST",
        credentials: "same-origin",
        headers: {
          "content-type": "application/json",
          "if-match": `"${digest}"`,
          "x-csrf-token": csrfToken(),
        },
        body: JSON.stringify(policy),
      })
      if (!response.ok) {
        const body = (await response.json().catch(() => null)) as {
          error?: string
        } | null
        throw new Error(
          body?.error ?? `Model policy update failed with ${response.status}`
        )
      }
      return response.json()
    },
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["control-plane"] })
    },
  })
}

export function useCapabilityModeUpdate() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: async ({
      capability,
      mode,
      evidenceDigest,
    }: {
      capability: CapabilityClass
      mode: CapabilityClass["mode"]
      evidenceDigest: string | null
    }) => {
      const response = await fetch(
        `/api/v1/capabilities/${encodeURIComponent(capability.id)}/mode`,
        {
          method: "POST",
          credentials: "same-origin",
          headers: {
            "content-type": "application/json",
            "if-match": `"${capability.definition_digest}"`,
            "x-csrf-token": csrfToken(),
          },
          body: JSON.stringify({
            mode,
            evidence_digest: evidenceDigest,
            confirm: true,
          }),
        }
      )
      if (!response.ok) {
        const body = (await response.json().catch(() => null)) as {
          error?: string
        } | null
        throw new Error(
          body?.error ?? `Capability update failed with ${response.status}`
        )
      }
      return response.json() as Promise<CapabilityClass>
    },
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["control-plane"] })
    },
  })
}

export function useProofReplay() {
  return useMutation({
    mutationFn: async (proofId: string) => {
      const response = await fetch(
        `/api/v1/proofs/${encodeURIComponent(proofId)}/replay`,
        {
          method: "POST",
          credentials: "same-origin",
          headers: {
            "content-type": "application/json",
            "x-csrf-token": csrfToken(),
          },
          body: "{}",
        }
      )
      if (!response.ok) {
        throw new Error(`Proof replay failed with ${response.status}`)
      }
      return response.json() as Promise<{
        proof_id: string
        structural_valid: boolean
        fresh: boolean
        contradiction_free: boolean
        replay_valid: boolean
        model_invoked: false
      }>
    },
  })
}

export function useChanges() {
  return useQuery({
    queryKey: ["changes"],
    queryFn: () => getJson<ChangeListItem[]>("/api/v1/changes"),
  })
}

export function useChange(changeId: string | undefined) {
  return useQuery({
    queryKey: ["changes", changeId],
    queryFn: () =>
      getJson<ChangeDetail>(`/api/v1/changes/${encodeURIComponent(changeId ?? "")}`),
    enabled: Boolean(changeId),
  })
}

export function useChangeDiff(changeId: string | undefined) {
  return useQuery({
    queryKey: ["changes", changeId, "diff"],
    queryFn: async () => {
      const response = await fetch(
        `/api/v1/changes/${encodeURIComponent(changeId ?? "")}/diff`,
        { credentials: "same-origin" }
      )
      if (!response.ok) {
        throw new Error(`Diff request failed with ${response.status}`)
      }
      return response.text()
    },
    enabled: Boolean(changeId),
  })
}

export function useApplyChange() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: async ({
      changeId,
      digest,
    }: {
      changeId: string
      digest: string
    }) => {
      const response = await fetch(
        `/api/v1/changes/${encodeURIComponent(changeId)}/apply`,
        {
          method: "POST",
          credentials: "same-origin",
          headers: {
            "content-type": "application/json",
            "if-match": `"${digest}"`,
            "x-csrf-token": csrfToken(),
          },
          body: JSON.stringify({ confirm: true }),
        }
      )
      if (!response.ok) {
        const body = (await response.json().catch(() => null)) as {
          error?: string
        } | null
        throw new Error(body?.error ?? `Apply failed with ${response.status}`)
      }
      return response.json() as Promise<ChangeApplyRecord>
    },
    onSuccess: async (_, variables) => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["changes"] }),
        queryClient.invalidateQueries({
          queryKey: ["changes", variables.changeId],
        }),
        queryClient.invalidateQueries({ queryKey: ["control-plane"] }),
      ])
    },
  })
}
