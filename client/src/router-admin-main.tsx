// The router's own admin pages (D151): a bundle of its own (vite.router.config.ts ->
// client/dist-router), embedded in the router binary and served at /admin/.
import React from "react";
import ReactDOM from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { TooltipProvider } from "@/components/ui/misc";
import { RouterAdminApp } from "@/features/router/admin/RouterAdminApp";
import "./index.css";

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: 1, refetchOnWindowFocus: true, staleTime: 1000 } },
});

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <TooltipProvider delayDuration={300}>
        <div className="h-screen bg-background text-foreground">
          <RouterAdminApp />
        </div>
      </TooltipProvider>
    </QueryClientProvider>
  </React.StrictMode>,
);
