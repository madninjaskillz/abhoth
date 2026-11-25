import { cn } from "@/lib/utils";

/**
 * The Abhoth mascot (a purple hooded creature). Served from /brand (public, no token needed, so
 * the token page can show it too). On dark surfaces it sits on a soft lavender glow (large).
 */
export function Mascot({ size = 128, className, glow = true }: { size?: 64 | 96 | 128 | 160; className?: string; glow?: boolean }) {
  const src = size <= 64 ? "/brand/mascot-128.webp" : "/brand/mascot-256.webp";
  return (
    <div className={cn("relative mx-auto", className)} style={{ width: size, height: size }} aria-hidden>
      {glow && <div className="mascot-glow absolute inset-[-14%] rounded-full" />}
      <img src={src} width={size} height={size} alt="" draggable={false} className="relative select-none" />
    </div>
  );
}

/** The small round logo for the top bar. */
export function MascotBadge({ className }: { className?: string }) {
  return <img src="/brand/badge-64.webp" width={22} height={22} alt="" aria-hidden draggable={false} className={cn("size-[22px] select-none", className)} />;
}

/** A tiny mascot beside "working…" while the agent streams: a slow fade, no movement. */
export function MascotThinking() {
  return <img src="/brand/badge-64.webp" width={16} height={16} alt="" aria-hidden draggable={false} className="mascot-thinking size-4 select-none" />;
}
