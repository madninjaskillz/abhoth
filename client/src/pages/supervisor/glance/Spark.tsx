import { memo, useRef, useState } from "react";
import type { Ring } from "./feed";
import { fmt, type Point } from "./model";

const W = 200;
/** The sparklines show the last 5 minutes (the dash's 120 samples at 2.5 s). */
export const WINDOW_S = 300;
const BUCKETS = 100;

export interface SparkProps {
  ring?: Ring;
  /** A second series on the same scale, dashed (HBM temperature). */
  ring2?: Ring;
  /** The feed's clock (state.time): the right edge. */
  now: number;
  /** Changes whenever the rings may have grown (the store version). */
  version: number;
  label: string;
  label2?: string;
  unit?: string;
  digits?: number;
  min0?: boolean;
  maxHint?: number;
  scale?: number;
  height?: number;
}

/**
 * A sparkline over a decimated window of a history ring (≤ 200 points however long the ring),
 * with a hover/touch crosshair. Screen readers get the latest value and the range.
 */
export const Spark = memo(function Spark({ ring, ring2, now, label, label2, unit = "", digits = 0, min0 = true, maxHint, scale = 1, height = 30 }: SparkProps) {
  const [hover, setHover] = useState<{ i: number; cx: number; cy: number } | null>(null);
  const ref = useRef<SVGSVGElement>(null);
  const from = now - WINDOW_S;
  const sc = (p: Point): Point => [p[0], p[1] == null ? null : p[1] * scale];
  const pts = (ring?.decimated(from, now, BUCKETS) ?? []).map(sc);
  const pts2 = (ring2?.decimated(from, now, BUCKETS) ?? []).map(sc);
  if (pts.length < 2) return <svg className="spark" height={height} aria-hidden />;
  const t0 = pts[0][0];
  const t1 = pts[pts.length - 1][0];
  const in2 = pts2.filter((p) => p[0] >= t0 && p[0] <= t1);
  const vals = pts.concat(in2).map((p) => p[1] as number);
  const lo = min0 ? 0 : Math.min(...vals);
  let hi = Math.max(...vals, maxHint ?? -Infinity);
  if (hi - lo < 1e-9) hi = lo + 1;
  const x = (t: number) => (t1 === t0 ? 0 : ((t - t0) / (t1 - t0)) * W);
  const y = (v: number) => height - 2 - ((v - lo) / (hi - lo)) * (height - 4);
  const path = (ps: Point[]) => ps.map((p, i) => (i ? "L" : "M") + x(p[0]).toFixed(1) + " " + y(p[1] as number).toFixed(1)).join("");
  const d = path(pts);
  const d2 = in2.length > 1 ? path(in2) : null;
  const last = pts[pts.length - 1][1] as number;
  const near = (ps: Point[], t: number) => ps.reduce<Point | null>((b, p) => (!b || Math.abs(p[0] - t) < Math.abs(b[0] - t) ? p : b), null);
  const move = (clientX: number) => {
    const r = ref.current?.getBoundingClientRect();
    if (!r || !r.width) return;
    const fx = ((clientX - r.left) / r.width) * W;
    let best = 0;
    pts.forEach((p, i) => {
      if (Math.abs(x(p[0]) - fx) < Math.abs(x(pts[best][0]) - fx)) best = i;
    });
    setHover({ i: best, cx: clientX, cy: r.top });
  };
  const hp = hover ? pts[Math.min(hover.i, pts.length - 1)] : null;
  const h2 = hp && d2 ? near(in2, hp[0]) : null;
  const summary = `${label} ${fmt(last, digits)}${unit}, range ${fmt(Math.min(...pts.map((p) => p[1] as number)), digits)} to ${fmt(Math.max(...pts.map((p) => p[1] as number)), digits)}${unit} over ${Math.round((t1 - t0) / 60)} min`;
  return (
    <div style={{ position: "relative" }}>
      <svg
        ref={ref}
        className="spark"
        viewBox={`0 0 ${W} ${height}`}
        preserveAspectRatio="none"
        height={height}
        role="img"
        aria-label={summary}
        onMouseMove={(e) => move(e.clientX)}
        onMouseLeave={() => setHover(null)}
        onTouchStart={(e) => move(e.touches[0].clientX)}
        onTouchEnd={() => setTimeout(() => setHover(null), 1200)}
      >
        <path className="area" d={`${d}L${W} ${height}L0 ${height}Z`} />
        <path className="line" d={d} vectorEffect="non-scaling-stroke" />
        {d2 && <path className="line2" d={d2} vectorEffect="non-scaling-stroke" />}
        {hp && <line className="cross" x1={x(hp[0])} x2={x(hp[0])} y1={0} y2={height} vectorEffect="non-scaling-stroke" />}
      </svg>
      {hp && hover && (
        <div className="spark-tip" style={{ left: Math.min(hover.cx + 10, window.innerWidth - 220), top: hover.cy - 26 }}>
          {label2 ? label + " " : ""}
          {fmt(hp[1], digits)}
          {unit}
          {h2 ? ` · ${label2 ?? ""} ${fmt(h2[1], digits)}${unit}` : ""} ·{" "}
          {new Date(hp[0] * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" })}
        </div>
      )}
    </div>
  );
});
