import React from "react";
import ReactDOM from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { TooltipProvider } from "@/components/ui/misc";
import { startHub } from "@/lib/hub";
import { loadAuth } from "@/lib/auth";
import { loadBranding } from "@/lib/brand";
import { SignIn } from "@/features/auth/SignIn";
import { Setup } from "@/features/auth/Setup";
import App from "./App";
import "./index.css";

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: 1, refetchOnWindowFocus: true, staleTime: 1000 } },
});

const root = ReactDOM.createRoot(document.getElementById("root")!);

// No users yet: the setup page. Not signed in: the sign-in page. Then the app (reloaded, so it starts clean).
const done = () => location.assign("/");
void Promise.all([loadAuth(), loadBranding()]).then(([auth]) => {
  if (auth.setup) {
    if (location.pathname !== "/setup") history.replaceState(null, "", "/setup");
    root.render(
      <React.StrictMode>
        <Setup state={auth} onDone={done} />
      </React.StrictMode>,
    );
    return;
  }
  if (auth.stage !== "ok") {
    root.render(
      <React.StrictMode>
        <SignIn state={auth} onDone={done} />
      </React.StrictMode>,
    );
    return;
  }
  if (location.pathname === "/setup") history.replaceState(null, "", "/");
  startHub(queryClient);
  root.render(
    <React.StrictMode>
      <QueryClientProvider client={queryClient}>
        <TooltipProvider>
          <App />
        </TooltipProvider>
      </QueryClientProvider>
    </React.StrictMode>,
  );
});
