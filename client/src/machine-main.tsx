// This machine (MACHINE.md, D155): its own page (machine.html), opened from the desktop app's
// Settings. A page of its own so the router admin forms' transport points at the in-process
// router (/api/admin/machine) without touching the app's router-mode admin pages.
import React from "react";
import ReactDOM from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { TooltipProvider } from "@/components/ui/misc";
import { setRouterTransport } from "@/features/router/routerApi";
import { csrfHeaders } from "@/lib/auth";
import { MachineApp } from "@/features/machine/MachineApp";
import "./index.css";

setRouterTransport({ base: "/api/admin/machine", headers: csrfHeaders, unauthorized: () => {} });

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: 1, refetchOnWindowFocus: true, staleTime: 1000 } },
});

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <TooltipProvider delayDuration={300}>
        <div className="h-screen bg-background text-foreground">
          <MachineApp />
        </div>
      </TooltipProvider>
    </QueryClientProvider>
  </React.StrictMode>,
);
