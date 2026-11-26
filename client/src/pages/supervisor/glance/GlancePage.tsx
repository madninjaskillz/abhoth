import { Fragment, memo, useEffect, useMemo, useState, useSyncExternalStore } from "react";
import { GlanceStore, connectFeed, gpuSeries, llmSeries, slotSeries, type History } from "./feed";
import { Spark } from "./Spark";
import { SwitchProgress } from "@/features/router/widgets";
import type { JobSummary } from "@/features/router/routerModel";
import {
  fromSections,
  ago, alertLevel, appName, cpuLevel, engineLevel, espState, fanText, fmt, gib, gpuForCard, imageBackendState, instancesOf,
  kTok, leaseText, memory, newsState, nicLevel, pills, poolFreeText, poolPct, poolsOrdered, rate, recentRequests, roleOf,
  rpm, serviceGroups, shortName, slotChip, tempLevel, throttles, totals, unetLabel, connLevel,
  type CardState, type GlanceState, type Gpu, type Instance, type Level,
} from "./model";
import { cardAction, type CardActionResult } from "./glanceApi";
import "./glance.css";

export interface GlanceProps {
  /** `/api/glance/stream` (signed in) or `/public/glance/stream` (the public listener). */
  streamUrl: string;
  /** Admins get the card controls. */
  admin?: boolean;
  /** The redacted public view: no controls at all. */
  publicView?: boolean;
  /** Close the page (inside the app). */
  onClose?: () => void;
}

function useGlance(streamUrl: string) {
  const store = useMemo(() => new GlanceStore({ derive: fromSections }), [streamUrl]);
  useEffect(() => connectFeed(store, { url: streamUrl }), [store, streamUrl]);
  const view = useSyncExternalStore(store.subscribe, store.getSnapshot);
  // "updated N s ago" ticks between messages.
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);
  return { store, view, now };
}

/** A running switch in the control plane's shape (🟨C's widget shows it); the legacy dash's job has no steps. */
const isJobSummary = (j: unknown): j is JobSummary =>
  !!j && typeof j === "object" && typeof (j as JobSummary).steps === "number" && typeof (j as JobSummary).started === "number";

const Dot = ({ level }: { level: Level }) => <span className={"dot " + level} aria-hidden />;

function Bar({ frac, level, label }: { frac: number | null | undefined; level?: string; label: string }) {
  const f = Math.max(0, Math.min(1, frac || 0));
  return (
    <div className={"bar " + (level || "")} role="meter" aria-label={label} aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(f * 100)}>
      <i style={{ width: (f * 100).toFixed(1) + "%" }} />
    </div>
  );
}

function Tile({ k, v, u, s, hero, children }: { k: string; v: string | number; u?: string; s?: string; hero?: boolean; children?: React.ReactNode }) {
  return (
    <section className={"tile" + (hero ? " hero" : "")} aria-label={k}>
      <div className="k">{k}</div>
      <div className="v">
        {v}
        {u && <span className="u">{u}</span>}
      </div>
      {s && <div className="s" title={s}>{s}</div>}
      {children}
    </section>
  );
}

// ------------------------------------------------------------------ GPU card

function SlotRow({ s, inst, lease, h, now, version }: { s: NonNullable<Instance["slots"]>[number]; inst: Instance; lease: string | null; h: History; now: number; version: number }) {
  const ring = h.get(slotSeries(inst, s.id));
  const busy = !!ring && ring.points(now - 300).some((p) => (p[1] ?? 0) > 0);
  const chip = slotChip(s);
  return (
    <div className="slot" role="row">
      <div className="lbl" role="rowheader">{s.label}</div>
      <div className={"chip " + s.state} title={chip} role="cell">{chip}</div>
      <div className="sparkcell" role="cell">{busy && <Spark ring={ring} now={now} version={version} label={`${s.label} tok/s`} height={18} unit=" t/s" digits={1} />}</div>
      <div className="tok" role="cell">{s.state === "generating" || (s.tok_s ?? 0) > 0 ? fmt(s.tok_s, 1) : "–"}{s.estimated ? "*" : ""}</div>
      <div className="ctx" role="cell" title="tokens in context">{s.prompt_tokens ? kTok(s.prompt_tokens + (s.generated || 0)) : ""}</div>
      {lease && <div className="lease" role="cell">{lease}</div>}
    </div>
  );
}

const GpuCard = memo(function GpuCard({ g, s, h, version }: { g: Gpu; s: GlanceState; h: History; version: number }) {
  const insts = instancesOf(g, s);
  const role = roleOf(g, s.llm || {});
  const tok = insts.reduce((a, i) => a + (i.tok_s || 0), 0);
  const ft = fanText(g, s.esp);
  const memFrac = g.memory_total_bytes ? (g.memory_used_bytes ?? 0) / g.memory_total_bytes : 0;
  const pFrac = g.enforced_power_limit_watts ? (g.power_draw_watts ?? 0) / g.enforced_power_limit_watts : 0;
  const thr = throttles(g);
  const leases = s.pig?.slots || {};
  const tl = tempLevel(g.temperature_gpu);
  const hl = tempLevel(g.temperature_memory, "hbm");
  const now = s.time;
  const util = (g.utilization_gpu_ratio || 0) * 100;
  const title = `GPU${g.index} · ${shortName(g.name)}`;
  return (
    <article className={"card" + (role.text === "idle" ? " idle" : "")} aria-label={`${title}, ${role.text}`}>
      <div className="card-h">
        <div>
          <h2 className="title">
            {title}
            <span className={"role" + (role.llm ? " llm" : "")}>{role.text}</span>
          </h2>
          <div className="meta num">{g.pci} · {g.short}{g.pstate != null ? " · P" + g.pstate : ""}</div>
        </div>
        {insts.length > 0 && (
          <div className="right">
            <div className="bigtok">{fmt(tok, 1)}<span className="u">tok/s</span></div>
            <div className="meta">
              {insts.reduce((a, i) => a + (i.running || 0), 0)} running
              {insts.some((i) => i.queued) ? ` · ${insts.reduce((a, i) => a + (i.queued || 0), 0)} queued` : ""}
            </div>
          </div>
        )}
      </div>
      {insts.map((i) => (
        <div key={i.key}>
          <div className="model">
            <b>{i.model || "?"}</b> · {i.engine_label || i.engine}{i.engine_version ? " " + i.engine_version : ""}{i.port ? ` · :${i.port}` : ""}{i.container ? ` · ${i.container}` : ""}
            {i.ctx ? ` · ctx ${kTok(i.ctx)}` : ""}{i.kv ? ` · kv ${i.kv}` : ""}
            {i.up === false && <> · <span className="err">down{i.error ? ` (${i.error})` : ""}</span></>}
          </div>
          {insts.length === 1 && <Spark ring={h.get(llmSeries(i))} now={now} version={version} label="tok/s" height={34} unit=" tok/s" digits={1} />}
          <div className="slots" role="table" aria-label={`${i.role} slots`}>
            {(i.slots || []).map((sl) => {
              const l = leases[sl.label];
              return <SlotRow key={String(sl.id)} s={sl} inst={i} lease={l ? leaseText(sl.label, l) : null} h={h} now={now} version={version} />;
            })}
          </div>
        </div>
      ))}
      {!insts.length && (g.procs || []).length > 0 && (
        <div className="model">
          {g.procs!.map((p, n) => (
            <div key={p.pid ?? n}>
              <b>{appName(p.container || p.process)}</b>{p.process ? ` · ${p.process}` : ""} · {fmt((p.mem_mib ?? 0) / 1024, 1)} GiB
            </div>
          ))}
        </div>
      )}
      {!insts.length && !(g.procs || []).length && <div className="model">No GPU processes</div>}
      <dl className="stats">
        <div className="stat">
          <dt>Temp core / HBM</dt>
          <dd>
            <Dot level={tl} />{fmt(g.temperature_gpu)}°C
            {g.temperature_memory != null && <><span className="muted">/</span><Dot level={hl} />{fmt(g.temperature_memory)}°C</>}
          </dd>
        </div>
        <div className="stat"><dt>Fan ({ft.sub})</dt><dd>{ft.text}</dd></div>
        <div className="stat">
          <dt>Power</dt>
          <dd>{fmt(g.power_draw_watts)} / {fmt(g.enforced_power_limit_watts)} W</dd>
          <Bar frac={pFrac} level={pFrac > 0.95 ? "warn" : ""} label="Power of limit" />
        </div>
        <div className="stat">
          <dt>VRAM</dt>
          <dd>{gib(g.memory_used_bytes)} / {gib(g.memory_total_bytes)} GiB</dd>
          <Bar frac={memFrac} level={memFrac > 0.985 ? "warn" : ""} label="VRAM used" />
        </div>
        <div className="stat">
          <dt>Util compute / mem BW</dt>
          <dd>{fmt(util)}% / {fmt((g.utilization_memory_ratio || 0) * 100)}%</dd>
          <Bar frac={g.utilization_gpu_ratio} label="Compute utilisation" />
        </div>
        <div className="stat">
          <dt>SM / mem clock</dt>
          <dd>{fmt((g.clocks_current_sm_clock_hz ?? NaN) / 1e6)}<span className="muted">/{fmt((g.clocks_max_sm_clock_hz ?? NaN) / 1e6)}</span> · {fmt((g.clocks_current_memory_clock_hz ?? NaN) / 1e6)} MHz</dd>
        </div>
        {g.clock && (
          <div className="stat" title="The clock controller (gpuctl): serving mode, idle phase, energy saved at idle">
            <dt>Clock control</dt>
            <dd className="small">{[g.clock.serving, g.clock.phase === "low" ? "idle-low" : g.clock.phase, g.clock.cap_mhz ? `cap ${g.clock.cap_mhz} MHz` : null, g.clock.pinned ? "pinned" : null].filter(Boolean).join(" · ")}{g.clock.saved_wh != null && <span className="muted"> · saved {fmt(g.clock.saved_wh, 1)} Wh</span>}</dd>
          </div>
        )}
        <div className="stat"><dt>PCIe</dt><dd>gen{fmt(g.pcie_link_gen_current)} ×{fmt(g.pcie_link_width_current)}</dd></div>
        <div className="stat">
          <dt>Throttle</dt>
          <dd>{thr.length ? <><Dot level="serious" /><span>{thr.join(", ")}</span></> : <span className="muted">none</span>}</dd>
        </div>
      </dl>
      <div className="sparks four">
        <div className="spark-wrap">
          <div className="k" title="core (solid) / HBM (dashed)">
            <span>Temp</span>
            <span className="num"><span className="key1">{fmt(g.temperature_gpu)}°</span>{g.temperature_memory != null && <>/<span className="key2">{fmt(g.temperature_memory)}°</span></>}</span>
          </div>
          <Spark ring={h.get(gpuSeries(g, "temperature_gpu"))} ring2={h.get(gpuSeries(g, "temperature_memory"))} now={now} version={version} label="core" label2="HBM" min0={false} unit="°C" />
        </div>
        <div className="spark-wrap">
          <div className="k"><span title={g.duct ? `duct fan ${g.duct.fan} (ESP32)` : "card fan"}>Fan</span><span className="num">{ft.duty != null ? fmt(ft.duty) + "%" : "–"}</span></div>
          <Spark ring={h.get(gpuSeries(g, "fan_duty"))} now={now} version={version} label="fan" unit="%" maxHint={100} />
        </div>
        <div className="spark-wrap">
          <div className="k"><span>Power</span><span className="num">{fmt(g.power_draw_watts)} W</span></div>
          <Spark ring={h.get(gpuSeries(g, "power_draw_watts"))} now={now} version={version} label="power" unit=" W" />
        </div>
        <div className="spark-wrap">
          <div className="k"><span>Util</span><span className="num">{fmt(util)}%</span></div>
          <Spark ring={h.get(gpuSeries(g, "utilization_gpu_ratio"))} now={now} version={version} label="util" unit="%" maxHint={100} scale={100} />
        </div>
      </div>
    </article>
  );
});

// ------------------------------------------------------------------ panels

function CardsPanel({ s, admin }: { s: GlanceState; admin: boolean }) {
  const cards = s.profile?.cards;
  const [pending, setPending] = useState<{ card: string; action: string } | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [msg, setMsg] = useState<string | null>(null);
  if (!cards || !Object.keys(cards).length) return null;
  const run = async (card: string, action: "online" | "offline") => {
    setPending(null);
    setBusy(card);
    await cardAction(card, action, (r: CardActionResult) => setMsg(r.text));
    setBusy(null);
  };
  return (
    <section className="panel" aria-labelledby="g-cards">
      <h2 id="g-cards">32 GB cards · online / offline</h2>
      {Object.values(cards).map((v: CardState) => {
        const g = gpuForCard(s, v);
        const action = v.offline ? "online" : "offline";
        const lvl: Level = v.offline ? "warning" : v.healthy ? "good" : "critical";
        return (
          <div className="cardrow" key={v.card}>
            <div><Dot level={lvl} /> <b>Card {v.card}</b> <span className="muted small">{v.gpu}</span></div>
            <div className="small t2">
              {v.offline ? <><b>OFFLINE</b>{v.since ? ` since ${new Date(v.since).toLocaleTimeString()}` : ""} · layout wants {v.layoutEngine}</> : `${v.engine} · ${v.slots} slots · ${v.detail}`}
            </div>
            <div className="small muted">{v.model || "–"}</div>
            <div className="small num t2">{g ? `VRAM ${gib(g.memory_used_bytes)} / ${gib(g.memory_total_bytes)} GiB · util ${fmt((g.utilization_gpu_ratio || 0) * 100)}%` : "no GPU metrics"}</div>
            {admin && (
              <div className="cardbtns">
                {pending?.card === v.card ? (
                  <>
                    <button className="btn danger" onClick={() => run(v.card, action)}>Confirm: take card {v.card} {action}</button>
                    <button className="btn" onClick={() => setPending(null)}>Cancel</button>
                  </>
                ) : (
                  <button className="btn" disabled={!!busy} onClick={() => setPending({ card: v.card, action })}>Take card {v.card} {action}</button>
                )}
              </div>
            )}
          </div>
        );
      })}
      <div className="small t2" aria-live="polite" style={{ marginTop: msg ? 6 : 0 }}>{msg}</div>
      {!admin && <div className="small muted">Read-only: admins can take cards online or offline.</div>}
    </section>
  );
}

function Services({ s }: { s: GlanceState }) {
  return (
    <section className="panel" aria-labelledby="g-svc">
      <h2 id="g-svc">Services</h2>
      <ul className="svc">
        {serviceGroups(s).map(([g, list]) => (
          <Fragment key={g}>
            <li className="grp" aria-hidden>{g}</li>
            {list.map((x) => (
              <li key={x.name} title={x.port ? ":" + x.port : ""}>
                <Dot level={x.up ? "good" : "critical"} />{x.name}
                <span className="muted small">{x.note || (x.up ? "" : " down")}</span>
                <span className="sr">{x.up ? " up" : ""}, {g}</span>
              </li>
            ))}
          </Fragment>
        ))}
      </ul>
    </section>
  );
}

function GpuFans({ s, h, version }: { s: GlanceState; h: History; version: number }) {
  const e = s.esp;
  const c = e?.cfg || {};
  const st = espState(e);
  const byPci = new Map((s.gpus || []).map((g) => [g.pci, g]));
  const noise = Object.entries(s.noise || {});
  const noiseRing = h.get("noise");
  return (
    <section className="panel" aria-labelledby="g-fans">
      <h2 id="g-fans">GPU fans · ESP32</h2>
      <div className="row" style={{ gap: "6px 12px", marginBottom: 6 }}>
        <span className="row" style={{ margin: 0 }}><Dot level={st.level} />{st.text}{!e?.live && e?.error ? <span className="muted small"> {e.error}</span> : null}</span>
        <span className="row" style={{ margin: 0 }}><Dot level={e?.failsafe ? "critical" : e?.failsafe === false ? "good" : ""} />failsafe {e?.failsafe != null ? (e.failsafe ? "ACTIVE" : "off") : "–"}</span>
        <span className="row" style={{ margin: 0 }}><Dot level={e?.manual ? "warning" : e?.manual === false ? "good" : ""} />{e?.manual ? `manual ${fmt(c["Manual Duty"])}%` : e?.manual === false ? "auto curve" : "mode –"}</span>
      </div>
      <div className="scroll">
        <table>
          <thead><tr><th scope="col">Fan</th><th scope="col">Card</th><th scope="col" className="r">Temp</th><th scope="col" className="r">Load</th><th scope="col" className="r">Duty</th><th scope="col" className="r">FF</th><th scope="col" className="r">RPM</th><th scope="col"><span className="sr">Stalled</span></th></tr></thead>
          <tbody>
            {(e?.fans || []).map((f) => {
              const g = byPci.get(f.pci);
              const ok = e?.live && f.rpm != null;
              return (
                <tr key={f.fan}>
                  <td>{f.fan}</td>
                  <td>{g ? shortName(g.name).replace(/^V100-/, "") : f.pci}</td>
                  <td className="r"><span style={{ marginRight: 4 }}><Dot level={ok ? tempLevel(f.temp) : ""} /></span>{fmt(f.temp, 1)}°</td>
                  <td className="r">{fmt(f.load)}%</td>
                  <td className="r">{fmt(f.duty)}%</td>
                  <td className="r muted">{fmt(f.ff_duty)}%</td>
                  <td className="r">{rpm(f.rpm)}</td>
                  <td>{f.stalled ? <span className="err">STALLED</span> : ""}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      <div className="sparks">
        {(e?.fans || []).map((f) => (
          <div className="spark-wrap" key={f.fan}>
            <div className="k"><span>Fan {f.fan} duty</span><span className="num">{fmt(f.duty)}%</span></div>
            <Spark ring={h.get(`fan:${f.fan}:duty`)} now={s.time} version={version} label={`fan ${f.fan} duty`} unit="%" maxHint={100} />
          </div>
        ))}
      </div>
      <div className="muted small" style={{ marginTop: 8 }}>
        curve {fmt(c["Curve Low Temp"])}→{fmt(c["Curve High Temp"])}°C · min {fmt(c["Min Duty"])}% · critical {fmt(c["Critical Temp"])}°C ·
        feed-forward {fmt(c["FF Low Load"])}→{fmt(c["FF High Load"])}% load up to {fmt(c["FF Max Duty"])}% ·
        WiFi {e?.wifi != null ? fmt(e.wifi) + " dBm" : "–"}{e?.uptime != null ? " · ESP up " + ago(e.uptime) : ""}
      </div>
      <div className="muted small">The ESP fails safe to 100% after 30 s without fresh data. Temp = HBM (core where a card has none). CPU fans are on the BMC.</div>
      {noise.length > 0 && (
        <dl className="kv" style={{ marginTop: 8 }}>
          {noise.map(([m, v]) => <Fragment key={m}><dt>Noise {m}</dt><dd>{fmt(v, 1)} dBFS</dd></Fragment>)}
        </dl>
      )}
      {noiseRing && noiseRing.len > 2 && (
        <div style={{ marginTop: 8 }}>
          <div className="muted small">Noise (loudest mic)</div>
          <Spark ring={noiseRing} now={s.time} version={version} label="noise" min0={false} unit=" dBFS" digits={1} />
        </div>
      )}
    </section>
  );
}

function Bmc({ s, h, version }: { s: GlanceState; h: History; version: number }) {
  const b = s.bmc;
  const live = !!b?.live;
  const T = (label: string, v: number | undefined, lvl: (c: number | undefined) => Level, key: string) => (
    <div className="spark-wrap" key={label}>
      <div className="k"><span><span style={{ marginRight: 4 }}><Dot level={live ? lvl(v) : ""} /></span>{label}</span><span className="num">{live ? fmt(v) + "°C" : "–"}</span></div>
      <Spark ring={h.get("bmc:" + key)} now={s.time} version={version} label={label} unit="°C" min0={false} />
    </div>
  );
  return (
    <section className="panel" aria-labelledby="g-bmc">
      <h2 id="g-bmc">CPU &amp; board · BMC</h2>
      <div className="row" style={{ gap: "6px 12px", marginBottom: 6 }}>
        <span className="row" style={{ margin: 0 }}><Dot level={live ? "good" : "critical"} />{live ? "live" : b ? "stale " + ago(b.age_s) : "no data"}{b?.error ? <span className="muted small"> {b.error}</span> : null}</span>
        <span className="row" style={{ margin: 0 }}><Dot level={b?.fan_mode ? (b.fan_mode === "Optimal" ? "good" : "warning") : ""} />fan mode {b?.fan_mode || "–"}</span>
      </div>
      <div className="sparks">
        {T("CPU1", b?.cpu1, cpuLevel, "cpu1")}
        {T("CPU2", b?.cpu2, cpuLevel, "cpu2")}
        {T("10G NIC", b?.nic, nicLevel, "nic")}
      </div>
      <dl className="kv" style={{ marginTop: 8 }}>
        {Object.entries(b?.fans || {}).map(([k, v]) => <Fragment key={k}><dt>{k} (CPU fan)</dt><dd>{live ? fmt(v) + " RPM" : "–"}</dd></Fragment>)}
        <dt>VRM / DIMM max</dt><dd>{live ? `${fmt(b!.vrm_max)}° / ${fmt(b!.dimm_max)}°C` : "–"}</dd>
        <dt>System / peripheral</dt><dd>{live ? `${fmt(b!.system)}° / ${fmt(b!.peripheral)}°C` : "–"}</dd>
      </dl>
      <div className="muted small" style={{ marginTop: 6 }}>NIC amber at 75°C, red at 85°C.</div>
    </section>
  );
}

function ImageGen({ s }: { s: GlanceState }) {
  const im = s.image;
  const comfy = (s.services || []).find((x) => x.name === "ComfyUI");
  return (
    <section className="panel" aria-labelledby="g-img">
      <h2 id="g-img">Image generation</h2>
      {!im ? (
        <div className="row"><Dot level="critical" />image-api not answering</div>
      ) : (
        <>
          <div className="row">
            <Dot level={im.status === "ok" ? "good" : "warning"} />image-api {im.status || "?"}
            {comfy && <><span className="grow" /><Dot level={comfy.up ? "good" : "critical"} />ComfyUI {comfy.up ? "up" : "down"}</>}
          </div>
          <div className="scroll">
            <table>
              <thead><tr><th scope="col">Backend</th><th scope="col">State</th><th scope="col">Model</th><th scope="col" className="r">Queue</th><th scope="col" className="r" title="served / failed">Done</th></tr></thead>
              <tbody>
                {(im.backends || []).map((b) => {
                  const st = imageBackendState(b);
                  return (
                    <tr key={b.name}>
                      <td>{b.name}</td>
                      <td style={{ whiteSpace: "nowrap" }}><span style={{ marginRight: 5 }}><Dot level={st.level} /></span>{st.text}</td>
                      <td title={(b.unet || "") + (b.steps ? ` · ${b.steps} steps` : "")} style={{ whiteSpace: "nowrap" }}>{unetLabel(b.unet)}</td>
                      <td className="r">{b.queue_running != null ? `${b.queue_running}+${b.queue_pending || 0}` : b.inflight ?? "–"}</td>
                      <td className="r">{b.served ?? "–"}{b.failed ? <span className="err">/{b.failed}</span> : ""}</td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </>
      )}
    </section>
  );
}

function Pools({ s }: { s: GlanceState }) {
  const pools = poolsOrdered(s);
  return (
    <section className="panel" aria-labelledby="g-pools">
      <h2 id="g-pools">Storage pools</h2>
      {pools.map((p) => {
        const pct = poolPct(p);
        return (
          <div key={p.name} style={{ marginBottom: 8 }}>
            <div className="row">
              <Dot level={p.health === "ONLINE" ? "good" : "critical"} /><b>{p.name}</b>
              <span className="sr">{p.health}</span>
              <span className="grow" /><span className="num">{pct}%</span>
              <span className="muted num small">{poolFreeText(p)}</span>
            </div>
            <Bar frac={pct / 100} level={pct >= 90 ? "crit" : pct >= 80 ? "warn" : ""} label={`${p.name} used`} />
          </div>
        );
      })}
      {!pools.length && <div className="muted">No pool data</div>}
    </section>
  );
}

function News({ s, nowMs }: { s: GlanceState; nowMs: number }) {
  const n = s.news;
  if (!n) return <section className="panel"><h2>AI News job</h2><div className="muted">No run logs found</div></section>;
  const st = newsState(n, new Date(nowMs).toISOString().slice(0, 10));
  return (
    <section className="panel" aria-labelledby="g-news">
      <h2 id="g-news">AI News job</h2>
      <div className="row"><Dot level={st.level} /><b>{n.date}</b><span className="t2">{st.text}</span>{st.stale && <span className="muted small">(no run today yet)</span>}</div>
      {n.title && <div className="t2 small" style={{ margin: "4px 0" }}>{n.title}</div>}
      {n.video && <div className="small"><a href={`https://studio.youtube.com/video/${encodeURIComponent(n.video)}/edit`} target="_blank" rel="noopener noreferrer">YouTube Studio</a></div>}
      <div className="muted small" style={{ marginTop: 4 }}>log {n.file} · updated {ago(nowMs / 1000 - (n.mtime ?? 0))} ago{n.errors ? ` · ${n.errors} error lines` : ""}</div>
      {n.last_error && <div className="err" style={{ marginTop: 4, overflowWrap: "anywhere" }}>{n.last_error}</div>}
    </section>
  );
}

function Ooms({ s, nowMs }: { s: GlanceState; nowMs: number }) {
  const rows = (s.ooms || []).slice().reverse();
  const day = rows.filter((r) => nowMs - Date.parse(r.time) < 86400000).length;
  return (
    <section className="panel" aria-labelledby="g-oom">
      <h2 id="g-oom">OOM kills · 7 days</h2>
      <div className="row"><Dot level={day ? "serious" : "good"} /><span>{day} in the last 24 h, {rows.length} shown</span></div>
      {rows.length > 0 && (
        <div className="scroll">
          <table>
            <thead><tr><th scope="col">When</th><th scope="col">Process</th><th scope="col">Container</th><th scope="col" className="r">anon MB</th></tr></thead>
            <tbody>
              {rows.slice(0, 8).map((r, n) => (
                <tr key={r.time + n}>
                  <td>{new Date(r.time).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" })}</td>
                  <td>{r.process ?? "–"}</td>
                  <td>{appName(r.container) || "–"}</td>
                  <td className="r">{r.anon_mb ?? "–"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

function Copies({ s, nowMs }: { s: GlanceState; nowMs: number }) {
  const insts = Object.values(s.llm || {});
  if (!insts.length) return null;
  return (
    <section className="panel" aria-labelledby="g-copies">
      <h2 id="g-copies">Model copies · since restart</h2>
      <div className="scroll">
        <table>
          <thead><tr><th scope="col">Copy</th><th scope="col" className="r">Reqs</th><th scope="col" className="r">Out tok</th><th scope="col" className="r">Decode</th><th scope="col" className="r">Draft acc.</th><th scope="col" className="r">Cache</th><th scope="col" className="r">Up</th></tr></thead>
          <tbody>
            {insts.map((i) => {
              const t = i.totals || {};
              return (
                <tr key={i.key}>
                  <td>{i.role}</td>
                  <td className="r">{t.requests ?? "–"}</td>
                  <td className="r">{kTok(t.output_tokens)}</td>
                  <td className="r">{t.decode_tok_s != null ? fmt(t.decode_tok_s, 1) : "–"}</td>
                  <td className="r">{t.draft_accept != null ? fmt(t.draft_accept * 100) + "%" : "–"}</td>
                  <td className="r">{i.cache ? `${i.cache.parked}/${i.cache.slots}` : "–"}</td>
                  <td className="r">{i.started_at ? ago(nowMs / 1000 - Date.parse(i.started_at) / 1000) : "–"}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function Requests({ s, nowMs }: { s: GlanceState; nowMs: number }) {
  const rows = recentRequests(s);
  if (!rows.length) return null;
  return (
    <section className="panel wide" aria-labelledby="g-req">
      <h2 id="g-req">Recent requests</h2>
      <div className="scroll">
        <table>
          <thead><tr><th scope="col">Ago</th><th scope="col">Copy</th><th scope="col" className="r">Prompt</th><th scope="col" className="r">Cached</th><th scope="col" className="r">Out</th><th scope="col" className="r">tok/s</th><th scope="col" className="r">TTFT</th><th scope="col" className="r">Time</th><th scope="col">End</th></tr></thead>
          <tbody>
            {rows.map((r, n) => (
              <tr key={n}>
                <td>{ago(nowMs / 1000 - r.time)}</td>
                <td>{r.role}</td>
                <td className="r">{kTok(r.prompt)}</td>
                <td className="r">{r.prompt ? fmt((100 * (r.reused || 0)) / r.prompt) + "%" : "–"}</td>
                <td className="r">{kTok(r.out)}</td>
                <td className="r">{fmt(r.tok_s, 1)}</td>
                <td className="r">{r.ttft_ms != null ? fmt(r.ttft_ms / 1000, 2) + " s" : "–"}</td>
                <td className="r">{fmt(r.duration_s, 1)} s</td>
                <td>{r.finish || ""}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

/** Engine lifecycle (new): each engine's state, and the feed's recent events (switch steps, alerts). */
function Engines({ s, store, nowMs }: { s: GlanceState; store: GlanceStore; nowMs: number }) {
  const engines = s.engines || [];
  const events = store.events.slice(-6).reverse();
  if (!engines.length && !events.length) return null;
  return (
    <section className="panel" aria-labelledby="g-eng">
      <h2 id="g-eng">Engines</h2>
      {engines.length > 0 && (
        <div className="scroll">
          <table>
            <thead><tr><th scope="col">Engine</th><th scope="col">Card</th><th scope="col">State</th><th scope="col" className="r">Since</th></tr></thead>
            <tbody>
              {engines.map((e) => (
                <tr key={e.id}>
                  <td title={e.kind}>{e.id}</td>
                  <td>{e.card ?? "–"}</td>
                  <td style={{ whiteSpace: "nowrap" }}>
                    <span style={{ marginRight: 5 }}><Dot level={engineLevel(e.state)} /></span>{e.state}
                    {e.progress != null ? ` ${fmt(e.progress * 100)}%` : ""}
                    {e.detail && <div className="muted small" style={{ whiteSpace: "normal" }}>{e.detail}</div>}
                  </td>
                  <td className="r">{e.since ? ago(nowMs / 1000 - Date.parse(e.since) / 1000) : "–"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {events.length > 0 && (
        <ul className="small t2" style={{ margin: "8px 0 0", paddingLeft: 16 }} aria-label="Recent events">
          {events.map((e, n) => (
            <li key={n}>{ago((nowMs - e.at) / 1000)} ago · {e.severity && e.severity !== "info" && <><Dot level={e.severity === "critical" ? "critical" : "warning"} />{" "}</>}{e.kind}{e.text ? `: ${e.text}` : ""}</li>
          ))}
        </ul>
      )}
    </section>
  );
}

function CalendarStrip({ s }: { s: GlanceState }) {
  const c = s.calendar;
  if (!c || (!c.now?.length && !c.next?.length)) return null;
  const time = (iso?: string) => (iso ? new Date(iso).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }) : "?");
  return (
    <div className="strip" aria-label="Calendar">
      {(c.now || []).map((e) => (
        <span key={"n" + e.id}><span className="lbl">Now</span> <b>{e.title}</b> {e.end ? <> until {time(e.end)}</> : null}{(e.badges || []).map((b) => <span className="badge" key={b}>{b}</span>)}</span>
      ))}
      {(c.next || []).slice(0, 2).map((e) => (
        <span key={"x" + e.id}><span className="lbl">Next</span> {e.title} {e.start ? time(e.start) : ""}{e.end ? `–${time(e.end)}` : ""}{(e.badges || []).map((b) => <span className="badge" key={b}>{b}</span>)}</span>
      ))}
    </div>
  );
}

function Alerts({ s, nowMs }: { s: GlanceState; nowMs: number }) {
  const a = s.alerts || [];
  if (!a.length) return null;
  return (
    <div className="alerts" role="region" aria-label="Alerts">
      {a.map((x) => (
        <div className={"alert " + x.severity} key={x.id}>
          <Dot level={alertLevel(x)} /><b>{x.title}</b>
          <span className="muted small">{x.severity}{x.since ? ` · ${ago(nowMs / 1000 - Date.parse(x.since) / 1000)}` : ""}</span>
          {x.detail && <span className="t2 small">{x.detail}</span>}
        </div>
      ))}
    </div>
  );
}

// ------------------------------------------------------------------ page

export function GlancePage({ streamUrl, admin = false, publicView = false, onClose }: GlanceProps) {
  const { store, view, now } = useGlance(streamUrl);
  const s = view.state;
  useEffect(() => {
    if (publicView) document.title = "Glance";
  }, [publicView]);
  if (!s)
    return (
      <div className="glance" data-testid="glance">
        <div className="boot" role="status">{view.error ? "Can't reach the feed: " + view.error : "Loading Glance…"}</div>
      </div>
    );
  const h = store.history;
  const v = view.version;
  const t = totals(s);
  const m = memory(s.host);
  const host = s.host || {};
  const age = (now - view.lastAt) / 1000;
  const lvl = connLevel(age, view.status !== "live");
  return (
    <div className="glance" data-testid="glance">
      <div className="wrap">
        <header className="top">
          <h1 className="brand">Glance</h1>
          {pills(s, now).map((p) => (
            <div key={p.kind} className={"pill" + (p.kind !== "layout" ? " plain" : "") + (p.off ? " off" : "")} title={p.title}>
              {p.kind !== "layout" && <Dot level={p.level} />}
              {p.text}
              {p.sub && <span className="sub">{p.sub}</span>}
            </div>
          ))}
          <div className="spacer" />
          <div className="conn" role="status" aria-live="off">
            <Dot level={lvl} />
            {view.status === "live" ? `updated ${fmt(Math.max(0, age))} s ago` : view.error || view.status} · {new Date(s.time * 1000).toLocaleTimeString()}
            {publicView && <span className="muted"> · public view</span>}
          </div>
          {onClose && <button className="btn" onClick={onClose} aria-label="Close Glance">Close</button>}
        </header>
        {isJobSummary(s.profile?.job) && <SwitchProgress job={s.profile.job} now={now} />}
        <CalendarStrip s={s} />
        <Alerts s={s} nowMs={now} />

        <div className="tiles">
          <Tile hero k="Total generation" v={fmt(t.tokS, 1)} u="tok/s" s={t.perCopy || "no model servers"}>
            <Spark ring={h.get("total_tok_s")} now={s.time} version={v} label="total" height={30} unit=" tok/s" digits={1} />
          </Tile>
          <Tile k="Requests running" v={t.running} u={`/ ${t.slots} slots`} s={(t.queued ? t.queued + " queued · " : "") + (t.leases != null ? t.leases + " leases held" : "pig-slots down")} />
          <Tile k="GPU power" v={fmt(t.gpuPower)} u="W" s={"hottest core " + fmt(t.hottest) + "°C"} />
          <Tile k="Host CPU" v={fmt(host.cpu)} u="%" s={host.load ? "load " + host.load.map((x) => x.toFixed(1)).join(" ") : ""} />
          <Tile k={`Host RAM · ${gib(m.total)} GiB total`} v={gib(m.used)} u="GiB used">
            <div className="memrows num" title="Used = processes etc. (excl. ZFS ARC). Consumed = Used + ARC = total − MemAvailable. Free = MemAvailable.">
              <span><i className="sw used" />used {gib(m.used)}</span>
              <span><i className="sw arc" />ARC {m.arc != null ? gib(m.arc) : "–"}</span>
              <span>consumed {gib(m.consumed)}</span>
              <span>free {gib(m.free)}</span>
            </div>
            <div className="bar seg" role="img" aria-label={`RAM: ${gib(m.used)} GiB used, ${gib(m.arc)} GiB ARC, ${gib(m.free)} GiB free`}>
              <i className="used" style={{ width: (m.total && m.used != null ? (m.used / m.total) * 100 : 0).toFixed(1) + "%" }} />
              <i className="arc" style={{ width: (m.total ? ((m.arc ?? 0) / m.total) * 100 : 0).toFixed(1) + "%" }} />
            </div>
          </Tile>
          <Tile k="Network" v={rate(host.net_rx)} s={`out ${rate(host.net_tx)} · up ${ago(host.uptime_s)}`} />
          {s.ecoflow && (
            <Tile k="EcoFlow AC out" v={fmt(s.ecoflow.watts)} u={s.ecoflow.unit || "W"} s={s.ecoflow.updated ? `HA · ${ago(now / 1000 - Date.parse(s.ecoflow.updated) / 1000)} ago` : "Home Assistant"} />
          )}
        </div>

        <div className="gpus">
          {(s.gpus || []).map((g) => <GpuCard key={g.uuid} g={g} s={s} h={h} version={v} />)}
        </div>

        <div className="panels">
          <CardsPanel s={s} admin={admin && !publicView} />
          <Services s={s} />
          <Engines s={s} store={store} nowMs={now} />
          <GpuFans s={s} h={h} version={v} />
          <Bmc s={s} h={h} version={v} />
          <ImageGen s={s} />
          <Pools s={s} />
          <News s={s} nowMs={now} />
          <Ooms s={s} nowMs={now} />
          <Copies s={s} nowMs={now} />
          {!publicView && <Requests s={s} nowMs={now} />}
        </div>
        {s.errors && Object.keys(s.errors).length > 0 && (
          <div className="err foot">Collector warnings: {Object.entries(s.errors).map(([k, e]) => k + ": " + e).join(" · ")}</div>
        )}
        {(s.hostjson_age_s ?? 0) > 180 && <div className="err foot">Host collector is {ago(s.hostjson_age_s)} stale (containers, pools, OOM kills)</div>}
        <div className="muted small foot">
          * ninfer reports one rate per server; lane rates are an even split. Slot tok/s comes from deltas of each slot's generated-token count.
          {s.redacted?.length ? ` Public view: ${s.redacted.join(", ")} hidden.` : ""}
        </div>
      </div>
    </div>
  );
}

export default GlancePage;
