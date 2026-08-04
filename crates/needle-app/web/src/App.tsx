import { lazy, Suspense } from "react"
import { BrowserRouter, Navigate, Route, Routes } from "react-router"

import { AppShell } from "@/components/app-shell"
import { Skeleton } from "@/components/ui/skeleton"

const OverviewPage = lazy(() => import("@/pages/overview-page"))
const ApprovalInboxPage = lazy(() => import("@/pages/approval-inbox-page"))
const ResourcePage = lazy(() => import("@/pages/resource-page"))
const ChangesPage = lazy(() => import("@/pages/changes-page"))
const SemanticResourcePage = lazy(
  () => import("@/pages/semantic-resource-page")
)

function PageFallback() {
  return (
    <div className="flex flex-col gap-4 p-6">
      <Skeleton className="h-9 w-52" />
      <Skeleton className="h-24 w-full" />
      <Skeleton className="h-80 w-full" />
    </div>
  )
}

export default function App() {
  return (
    <BrowserRouter>
      <AppShell>
        <Suspense fallback={<PageFallback />}>
          <Routes>
            <Route path="/" element={<Navigate to="/overview" replace />} />
            <Route path="/overview" element={<OverviewPage />} />
            <Route path="/approvals" element={<ApprovalInboxPage />} />
            <Route path="/changes" element={<ChangesPage />} />
            <Route path="/changes/:id" element={<ChangesPage />} />
            <Route path="/needs" element={<SemanticResourcePage resource="needs" />} />
            <Route path="/subjects" element={<SemanticResourcePage resource="subjects" />} />
            <Route path="/contracts" element={<SemanticResourcePage resource="contracts" />} />
            <Route path="/plans" element={<SemanticResourcePage resource="plans" />} />
            <Route path="/proofs" element={<SemanticResourcePage resource="proofs" />} />
            <Route
              path="/capabilities"
              element={<SemanticResourcePage resource="capabilities" />}
            />
            <Route path="/:resource" element={<ResourcePage />} />
          </Routes>
        </Suspense>
      </AppShell>
    </BrowserRouter>
  )
}
