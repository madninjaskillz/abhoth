import { create } from "zustand";
import { cn } from "@/lib/utils";
import { moodLabel, moodsEnabled, setMoodsEnabled, type Mood } from "@/lib/moods";
// Imported, not in public/: the build gives each file a content-hashed name (assets/…-<hash>.webp),
// so no browser can show a cached copy of an older image that was served at the same path
// (the OpenOrca build served its orca at /brand/mascot-256.webp).
import mascot128 from "@/assets/brand/abhoth-mascot-128.webp";
import mascot256 from "@/assets/brand/abhoth-mascot-256.webp";
import badge64 from "@/assets/brand/abhoth-badge-64.webp";
import happy64 from "@/assets/brand/moods/abhoth-mood-happy-64.webp";
import happy128 from "@/assets/brand/moods/abhoth-mood-happy-128.webp";
import happy256 from "@/assets/brand/moods/abhoth-mood-happy-256.webp";
import sad64 from "@/assets/brand/moods/abhoth-mood-sad-64.webp";
import sad128 from "@/assets/brand/moods/abhoth-mood-sad-128.webp";
import sad256 from "@/assets/brand/moods/abhoth-mood-sad-256.webp";
import angry64 from "@/assets/brand/moods/abhoth-mood-angry-64.webp";
import angry128 from "@/assets/brand/moods/abhoth-mood-angry-128.webp";
import angry256 from "@/assets/brand/moods/abhoth-mood-angry-256.webp";
import surprised64 from "@/assets/brand/moods/abhoth-mood-surprised-64.webp";
import surprised128 from "@/assets/brand/moods/abhoth-mood-surprised-128.webp";
import surprised256 from "@/assets/brand/moods/abhoth-mood-surprised-256.webp";
import wink64 from "@/assets/brand/moods/abhoth-mood-wink-64.webp";
import wink128 from "@/assets/brand/moods/abhoth-mood-wink-128.webp";
import wink256 from "@/assets/brand/moods/abhoth-mood-wink-256.webp";
import sleepy64 from "@/assets/brand/moods/abhoth-mood-sleepy-64.webp";
import sleepy128 from "@/assets/brand/moods/abhoth-mood-sleepy-128.webp";
import sleepy256 from "@/assets/brand/moods/abhoth-mood-sleepy-256.webp";

/** Each face at 64, 128 and 256 px (same framing as the neutral mascot, so a swap doesn't jump). */
const MOOD_SRC: Record<Mood, [string, string, string]> = {
  happy: [happy64, happy128, happy256],
  sad: [sad64, sad128, sad256],
  angry: [angry64, angry128, angry256],
  surprised: [surprised64, surprised128, surprised256],
  wink: [wink64, wink128, wink256],
  sleepy: [sleepy64, sleepy128, sleepy256],
};

/** The image for a display size (2x for sharp edges on high-DPI screens). */
function moodSrc(mood: Mood, size: number): string {
  const [s64, s128, s256] = MOOD_SRC[mood];
  return size <= 32 ? s64 : size <= 64 ? s128 : s256;
}

/** Moods on or off (on by default; remembered in this browser). */
export const useMoods = create<{ on: boolean; set: (on: boolean) => void }>((set) => ({
  on: moodsEnabled(typeof localStorage === "undefined" ? undefined : localStorage),
  set: (on) => {
    setMoodsEnabled(typeof localStorage === "undefined" ? undefined : localStorage, on);
    set({ on });
  },
}));

/**
 * The Abhoth mascot (a purple hooded creature). On dark surfaces it sits on a soft lavender glow
 * (large). `mood`: one of its faces in place of the neutral one, faded in when it changes.
 */
export function Mascot({ size = 128, className, glow = true, mood }: { size?: 64 | 96 | 128 | 160; className?: string; glow?: boolean; mood?: Mood | null }) {
  const src = mood ? moodSrc(mood, size) : size <= 64 ? mascot128 : mascot256;
  return (
    <div className={cn("relative mx-auto", className)} style={{ width: size, height: size }} aria-hidden data-mood={mood ?? undefined}>
      {glow && <div className="mascot-glow absolute inset-[-14%] rounded-full" />}
      <img key={src} src={src} width={size} height={size} alt="" draggable={false} className={cn("relative select-none", mood && "mood-in")} />
    </div>
  );
}

/** A small face: the mood at the end of a response, the working mascot's live mood. */
export function MoodBadge({ mood, size = 20, title, className }: { mood: Mood; size?: number; title?: string; className?: string }) {
  return (
    <img
      key={mood}
      src={moodSrc(mood, size)}
      width={size}
      height={size}
      alt={`Abhoth is ${moodLabel(mood)}`}
      title={title}
      draggable={false}
      className={cn("mood-in shrink-0 select-none", className)}
      style={{ width: size, height: size }}
      data-testid="mood-badge"
      data-mood={mood}
    />
  );
}

/** The small round logo for the top bar. */
export function MascotBadge({ className }: { className?: string }) {
  return <img src={badge64} width={22} height={22} alt="" aria-hidden draggable={false} className={cn("size-[22px] select-none", className)} />;
}

/** A tiny mascot beside "working…" while the agent streams: a slow fade, no movement. `mood`: its live face. */
export function MascotThinking({ mood }: { mood?: Mood | null }) {
  if (mood) return <MoodBadge mood={mood} size={16} className="mascot-thinking size-4" />;
  return <img src={badge64} width={16} height={16} alt="" aria-hidden draggable={false} className="mascot-thinking size-4 select-none" />;
}
