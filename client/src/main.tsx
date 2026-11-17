import React from "react";
import ReactDOM from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { TooltipProvider } from "@/components/ui/misc";
import { startHub } from "@/lib/hub";
import { loadAuth } from "@/lib/auth";
import { SignIn } from "@/features/auth/SignIn";
import App from "./App";
import "./index.css";

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: 1, refetchOnWindowFocus: true, staleTime: 1000 } },
});

const root = ReactDOM.createRoot(document.getElementById("root")!);

// Sign-in mode: the sign-in page until there is a session; then the app (reloaded, so it starts clean).
void loadAuth().then((auth) => {
  if (auth.mode === "login" && auth.stage !== "ok") {
    root.render(
      <React.StrictMode>
        <SignIn state={auth} onDone={() => location.reload()} />
      </React.StrictMode>,
    );
    return;
  }
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
