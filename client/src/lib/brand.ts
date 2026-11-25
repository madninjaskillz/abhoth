/**
 * The product name and mascot styling: `/brand/branding.json` (public, like the mascot images
 * beside it in public/brand). The product is Abhoth; a host can still serve its own file there.
 * Loaded once before the first render; a missing or broken file keeps the defaults.
 */
export interface Branding {
  /** The product name in titles, placeholders and help text. */
  name: string;
  /** CSS background for the soft glow behind the large mascot (optional). */
  glow?: string;
}

const defaults: Branding = { name: "Abhoth" };
let current: Branding = defaults;

export function brand(): Branding {
  return current;
}

/** The product name ("Abhoth" unless the host says otherwise). */
export function brandName(): string {
  return current.name;
}

export async function loadBranding(): Promise<Branding> {
  try {
    const res = await fetch("/brand/branding.json", { cache: "no-cache" });
    if (res.ok) {
      const data = (await res.json()) as Partial<Branding>;
      if (typeof data.name === "string" && data.name.trim()) {
        current = { ...defaults, ...data, name: data.name.trim() };
      }
    }
  } catch {
    // keep the defaults
  }
  if (typeof document !== "undefined") {
    document.title = current.name;
    if (current.glow) document.documentElement.style.setProperty("--mascot-glow", current.glow);
  }
  return current;
}
