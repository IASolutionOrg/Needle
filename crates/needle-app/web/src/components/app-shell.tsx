import type { CSSProperties, ReactNode } from "react"
import {
  Activity,
  Bot,
  Braces,
  Boxes,
  CircleGauge,
  Database,
  FileCheck2,
  FilePenLine,
  Fingerprint,
  FlaskConical,
  GitBranch,
  Inbox,
  Layers3,
  Network,
  PlayCircle,
  Route,
  Settings,
  ShieldCheck,
  SlidersHorizontal,
} from "lucide-react"
import { NavLink, useLocation } from "react-router-dom"

import { useApprovalEvents, useControlPlane } from "@/api"
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarHeader,
  SidebarInset,
  SidebarMenu,
  SidebarMenuBadge,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarProvider,
  SidebarRail,
  SidebarTrigger,
} from "@/components/ui/sidebar"

const navigation = [
  { label: "Overview", path: "/overview", icon: CircleGauge },
  { label: "Platforms", path: "/platforms", icon: Layers3 },
  { label: "Agents", path: "/agents", icon: Bot },
  { label: "Routes & Plans", path: "/routes", icon: GitBranch },
  { label: "Needs", path: "/needs", icon: Braces },
  { label: "Subjects", path: "/subjects", icon: Fingerprint },
  { label: "Contracts", path: "/contracts", icon: Network },
  { label: "Plans", path: "/plans", icon: Route },
  { label: "Proofs", path: "/proofs", icon: FileCheck2 },
  { label: "Capability Promotion", path: "/capabilities", icon: ShieldCheck },
  { label: "Changes", path: "/changes", icon: FilePenLine },
  { label: "Artifacts", path: "/artifacts", icon: Boxes },
  { label: "Cache", path: "/cache", icon: Database },
  { label: "Runs", path: "/runs", icon: PlayCircle },
  { label: "Models", path: "/models", icon: SlidersHorizontal },
  { label: "Experiments", path: "/experiments", icon: FlaskConical },
  { label: "Settings", path: "/settings", icon: Settings },
  { label: "Approval Inbox", path: "/approvals", icon: Inbox },
] as const

export function AppShell({ children }: { children: ReactNode }) {
  const location = useLocation()
  const control = useControlPlane()
  useApprovalEvents()

  return (
    <SidebarProvider
      style={
        {
          "--sidebar-width": "15.5rem",
          "--sidebar-width-icon": "3.25rem",
        } as CSSProperties
      }
    >
      <Sidebar collapsible="icon" className="border-sidebar-border">
        <SidebarHeader className="h-15 justify-center border-b border-sidebar-border px-3">
          <div className="flex items-center gap-3 overflow-hidden">
            <div className="flex size-7 shrink-0 items-center justify-center border border-primary font-mono text-sm text-primary">
              N
            </div>
            <div className="min-w-0 group-data-[collapsible=icon]:hidden">
              <div className="truncate text-sm font-semibold">Needle</div>
              <div className="truncate text-xs text-muted-foreground">
                Control Plane
              </div>
            </div>
          </div>
        </SidebarHeader>
        <SidebarContent>
          <SidebarGroup className="py-3">
            <SidebarGroupContent>
              <SidebarMenu>
                {navigation.map((item) => (
                  <SidebarMenuItem key={item.path}>
                    <SidebarMenuButton
                      asChild
                      isActive={
                        location.pathname === item.path ||
                        location.pathname.startsWith(`${item.path}/`)
                      }
                      tooltip={item.label}
                      className="h-10 rounded-none px-3 text-[13px] data-active:border-l-2 data-active:border-primary data-active:bg-sidebar-accent"
                    >
                      <NavLink to={item.path}>
                        <item.icon />
                        <span>{item.label}</span>
                      </NavLink>
                    </SidebarMenuButton>
                    {item.path === "/approvals" &&
                    (control.data?.pending_approvals ?? 0) > 0 ? (
                      <SidebarMenuBadge>
                        {control.data?.pending_approvals}
                      </SidebarMenuBadge>
                    ) : null}
                  </SidebarMenuItem>
                ))}
              </SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
        </SidebarContent>
        <SidebarFooter className="border-t border-sidebar-border p-3">
          <div className="flex items-center gap-2 text-xs text-muted-foreground group-data-[collapsible=icon]:hidden">
            <Activity className="text-success" />
            Local runtime
          </div>
        </SidebarFooter>
        <SidebarRail />
      </Sidebar>
      <SidebarInset className="w-auto min-w-0">
        <header className="sticky top-0 z-10 flex h-15 items-center justify-between border-b bg-background/95 px-4 backdrop-blur md:px-6">
          <div className="flex items-center gap-3">
            <SidebarTrigger className="md:hidden" />
            <div className="flex items-center gap-2 text-xs text-muted-foreground">
              <span className="size-2 rounded-full bg-success" />
              <span className="text-foreground">Runtime healthy</span>
            </div>
          </div>
          <div className="flex items-center gap-4 text-xs">
            <span className="text-muted-foreground">Local profile</span>
            <span className="hidden font-mono text-link sm:inline">
              {window.location.host}
            </span>
          </div>
        </header>
        {children}
      </SidebarInset>
    </SidebarProvider>
  )
}
