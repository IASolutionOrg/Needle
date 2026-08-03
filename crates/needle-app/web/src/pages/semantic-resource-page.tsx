import {
  Braces,
  FileCheck2,
  Fingerprint,
  Network,
  Route,
  ShieldCheck,
} from "lucide-react"
import { useState } from "react"

import {
  type CapabilityClass,
  type SufficiencyProof,
  useCapabilityModeUpdate,
  useControlPlane,
  useProofReplay,
} from "@/api"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"

type SemanticResource =
  | "needs"
  | "subjects"
  | "contracts"
  | "plans"
  | "proofs"
  | "capabilities"

const definitions = {
  needs: {
    title: "Needs",
    description: "Canonical NeedIR, obligations, worlds, and residual intent.",
    icon: Braces,
  },
  subjects: {
    title: "Subjects",
    description: "Exact repository-scoped semantic subjects.",
    icon: Fingerprint,
  },
  contracts: {
    title: "Contracts",
    description: "Immutable predicate and route definitions used by validators.",
    icon: Network,
  },
  plans: {
    title: "Plans",
    description: "Validity-first selected plans and measured economics.",
    icon: Route,
  },
  proofs: {
    title: "Proofs",
    description: "Replayable sufficiency certificates. Replay never invokes a model.",
    icon: FileCheck2,
  },
  capabilities: {
    title: "Capability Promotion",
    description: "Explicit Shadow, Advisory, and Authoritative rollout controls.",
    icon: ShieldCheck,
  },
} as const

export default function SemanticResourcePage({
  resource,
}: {
  resource: SemanticResource
}) {
  const control = useControlPlane()
  const definition = definitions[resource]
  const Icon = definition.icon
  const semantic = control.data?.semantic

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

      {resource === "needs" ? (
        <div className="grid gap-4">
          <DataTable
            headers={["Need", "Required", "Preferred", "Residual", "World"]}
            rows={(semantic?.needs ?? []).map((need) => [
              need.id,
              need.required.map((item) => item.predicate).join(", "),
              need.preferred.map((item) => item.predicate).join(", ") || "—",
              need.residual?.mandatory ? need.residual.reason : "None",
              `${need.world.source_selector} / ${need.world.features}`,
            ])}
          />
          <DataTable
            headers={[
              "Session / step",
              "Relation",
              "Coordination",
              "Satisfied / missing",
              "Delivery",
              "Taint",
            ]}
            rows={(semantic?.need_steps ?? []).map(({ session_id, step }) => [
              `${session_id} / ${step.ordinal}`,
              step.relation,
              step.coordination,
              `${step.satisfied.length} / ${step.missing.length}`,
              step.delivery ?? step.state,
              step.main_discovery_tainted ? "Main discovery" : "Clean",
            ])}
          />
        </div>
      ) : resource === "subjects" ? (
        <DataTable
          headers={["Kind", "Canonical name", "Repository", "Subject ID"]}
          rows={(semantic?.subjects ?? []).map((subject) => [
            subject.kind,
            subject.canonical_name,
            subject.repository_lineage,
            subject.id,
          ])}
        />
      ) : resource === "contracts" ? (
        <DataTable
          headers={["Type", "Contract", "Allowed / required", "Definition digest"]}
          rows={[
            ...(semantic?.predicate_contracts ?? []).map((contract) => [
              "Predicate",
              contract.predicate,
              contract.allowed_facets.join(", "),
              contract.definition_digest,
            ]),
            ...(semantic?.route_contracts ?? []).map((contract) => [
              "Route",
              contract.route,
              contract.required.map((item) => item.predicate).join(", "),
              contract.definition_digest,
            ]),
          ]}
        />
      ) : resource === "plans" ? (
        <DataTable
          headers={[
            "Resolution",
            "Reuse units",
            "Covered",
            "Missing",
            "Net value",
            "Proof latency",
            "Allocations",
            "Plan ID",
          ]}
          rows={(semantic?.selected_plans ?? []).map((plan) => {
            const accounting = semantic?.proof_accounting.find(
              (record) => record.plan_id === plan.id
            )
            const latency =
              (accounting?.lookup_micros ?? 0) +
              (accounting?.validation_micros ?? 0) +
              (accounting?.planning_micros ?? 0) +
              (accounting?.projection_micros ?? 0)
            return [
              plan.decision_reason,
              `${plan.artifact_ids.length} artifact / ${(plan.claim_ids ?? []).length} claim`,
              `0x${plan.covered_mask.toString(16)}`,
              `0x${plan.missing_mask.toString(16)}`,
              plan.economics.expected_net_microusd == null
                ? "Advisory"
                : `${plan.economics.expected_net_microusd} μUSD`,
              `${latency} μs`,
              accounting?.allocation_count == null
                ? "Not sampled"
                : String(accounting.allocation_count),
              plan.id,
            ]
          })}
        />
      ) : resource === "proofs" ? (
        <ProofTable proofs={semantic?.proofs ?? []} />
      ) : (
        <CapabilityTable capabilities={semantic?.capabilities ?? []} />
      )}
    </div>
  )
}

function DataTable({
  headers,
  rows,
}: {
  headers: string[]
  rows: string[][]
}) {
  return (
    <section className="border bg-panel">
      <Table>
        <TableHeader>
          <TableRow>
            {headers.map((header) => (
              <TableHead key={header}>{header}</TableHead>
            ))}
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.map((row, index) => (
            <TableRow key={`${row[0]}-${index}`}>
              {row.map((cell, cellIndex) => (
                <TableCell
                  key={`${cellIndex}-${cell}`}
                  className={cellIndex === row.length - 1 ? "max-w-72 truncate font-mono" : ""}
                >
                  {cell}
                </TableCell>
              ))}
            </TableRow>
          ))}
        </TableBody>
      </Table>
      {rows.length === 0 ? (
        <p className="p-8 text-center text-sm text-muted-foreground">
          No recorded data yet.
        </p>
      ) : null}
    </section>
  )
}

function ProofTable({
  proofs,
}: {
  proofs: SufficiencyProof[]
}) {
  const replay = useProofReplay()
  return (
    <section className="border bg-panel">
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>Proof</TableHead>
            <TableHead>Obligations</TableHead>
            <TableHead>Artifacts</TableHead>
            <TableHead>Replay</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {proofs.map((proof) => (
            <TableRow key={proof.id}>
              <TableCell className="max-w-72 truncate font-mono">
                {proof.id}
              </TableCell>
              <TableCell>{proof.obligations.length}</TableCell>
              <TableCell>{proof.artifacts.length}</TableCell>
              <TableCell>
                <Button
                  size="sm"
                  variant="outline"
                  disabled={replay.isPending}
                  onClick={() => replay.mutate(proof.id)}
                >
                  Replay
                </Button>
                {replay.data?.proof_id === proof.id ? (
                  <Badge className="ml-2" variant="outline">
                    {replay.data.replay_valid ? "Valid" : "Rejected"}
                  </Badge>
                ) : null}
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </section>
  )
}

function CapabilityTable({
  capabilities,
}: {
  capabilities: CapabilityClass[]
}) {
  return (
    <div className="grid gap-4">
      {capabilities.map((capability) => (
        <CapabilityRow key={capability.id} capability={capability} />
      ))}
      {capabilities.length === 0 ? (
        <p className="border bg-panel p-8 text-center text-sm text-muted-foreground">
          No capability definitions are available.
        </p>
      ) : null}
    </div>
  )
}

function CapabilityRow({ capability }: { capability: CapabilityClass }) {
  const [evidenceDigest, setEvidenceDigest] = useState("")
  const update = useCapabilityModeUpdate()
  const setMode = (mode: CapabilityClass["mode"]) =>
    update.mutate({
      capability,
      mode,
      evidenceDigest:
        mode === "advisory" || mode === "authoritative"
          ? evidenceDigest || null
          : null,
    })

  return (
    <section className="border bg-panel p-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <div className="font-mono text-sm">{capability.id}</div>
          <div className="mt-1 text-xs text-muted-foreground">
            {capability.reuse_unit ?? "artifact"} / {capability.predicate} / exact
            subject / positive / single world
          </div>
        </div>
        <Badge variant="outline">{capability.mode}</Badge>
      </div>
      <div className="mt-4 flex flex-wrap gap-2">
        <Input
          className="min-w-72 flex-1 font-mono text-xs"
          placeholder="Evidence digest required for Advisory / Authoritative"
          value={evidenceDigest}
          onChange={(event) => setEvidenceDigest(event.target.value)}
        />
        {(["shadow", "advisory", "authoritative"] as const).map((mode) => (
          <Button
            key={mode}
            size="sm"
            variant={capability.mode === mode ? "default" : "outline"}
            disabled={
              update.isPending ||
              ((mode === "advisory" || mode === "authoritative") &&
                evidenceDigest.length === 0)
            }
            onClick={() => setMode(mode)}
          >
            {mode}
          </Button>
        ))}
      </div>
      {update.error ? (
        <p className="mt-2 text-xs text-destructive">{update.error.message}</p>
      ) : null}
    </section>
  )
}
