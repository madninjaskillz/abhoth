import { useQuery } from "@tanstack/react-query";
import { api } from "@/lib/api";
import { inDesktop } from "@/lib/desktop";

/** What each named part of Abhoth does (docs/NAMES.md), in the words the settings use. */
export const PART_NAMES = [
  { name: "Sothoth Router", was: "the router", what: "The gateway every model request passes through. It knows the GPU servers, their cards and models, and hands out keys." },
  { name: "Migo Bridge", was: "the bridge", what: "Brings another machine in: it dials your Sothoth Router and shares that machine's GPUs and engines with it. No port opens on the machine." },
  { name: "Nodens Supervisor", was: "the supervisor", what: "Watches the GPU hosts: fans, temperatures, clocks and card health. Its live feed is the Glance page." },
];

/** Settings → About: the version, and what Sothoth Router, Migo Bridge and Nodens Supervisor are. */
export function AboutPane() {
  const info = useQuery({ queryKey: ["info"], queryFn: api.info, staleTime: 60000 });
  return (
    <div className="space-y-4 p-4 text-sm" data-testid="settings-about">
      <section className="space-y-1">
        <h3 className="text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">Abhoth</h3>
        <div>
          Version <span className="font-mono" data-testid="about-version">{info.data?.version ?? "…"}</span>
          <span className="text-muted-foreground"> · {inDesktop() ? "desktop app" : "web UI"}</span>
        </div>
      </section>
      <section className="space-y-2">
        <h3 className="text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">The parts and their names</h3>
        <dl className="space-y-2">
          {PART_NAMES.map((p) => (
            <div key={p.name}>
              <dt className="font-medium">
                {p.name} <span className="text-xs font-normal text-muted-foreground">({p.was})</span>
              </dt>
              <dd className="text-xs text-muted-foreground">{p.what}</dd>
            </div>
          ))}
        </dl>
      </section>
    </div>
  );
}
