// Which media files a chat item mentions, and where in a turn each one is previewed. Free of React
// and of path-alias runtime imports (type imports only), so plain node can test it
// (test/media.test.ts).
import type { ChatItem } from "../../store/session";

const MEDIA: Record<string, "image" | "pdf" | "audio" | "video"> = {
  png: "image", jpg: "image", jpeg: "image", gif: "image", webp: "image", bmp: "image", ico: "image", avif: "image", svg: "image",
  pdf: "pdf",
  mp3: "audio", wav: "audio", ogg: "audio", oga: "audio", flac: "audio", m4a: "audio", aac: "audio", opus: "audio",
  mp4: "video", m4v: "video", webm: "video", mov: "video", ogv: "video",
};

export function mediaKind(path: string) {
  const ext = path.split(".").pop()?.toLowerCase() ?? "";
  return MEDIA[ext];
}

/**
 * Whether a token in chat output reads as a file path: no spaces or URL scheme, and either a
 * folder separator or a file extension. ":12" / ":12:3" line suffixes are allowed (and dropped).
 */
export function asPath(token: string): string | null {
  const t = token.trim().replace(/^["'`]|["'`]$/g, "");
  if (!t || t.length > 300 || /\s/.test(t) || /^[a-z][a-z0-9+.-]*:\/\//i.test(t)) return null;
  const m = /^((?:[A-Za-z]:[\\/]|~?[\\/]|\.{1,2}[\\/])?[\w@.+\-[\]()]+(?:[\\/][\w@.+\-[\]() ]*[\w@.+\-[\]()])*)(?::\d+(?::\d+)?)?$/.exec(t);
  if (!m) return null;
  const p = m[1];
  const hasSep = /[\\/]/.test(p);
  const hasExt = /\.[A-Za-z0-9]{1,8}$/.test(p) && !/^\d+(\.\d+)+$/.test(p) && !/^v?\d/.test(p.split(/[\\/]/).pop()!);
  if (!hasSep && !hasExt) return null;
  if (hasSep && !hasExt && !/[\\/]$/.test(p) && p.split(/[\\/]/).length < 2) return null;
  return p;
}

/** The path relative to <paramref name="root"/> when inside it ("/" separators), else null for absolute paths outside. */
export function relativeTo(root: string, path: string): string | null {
  const norm = (s: string) => s.replace(/\\/g, "/").replace(/\/+$/, "");
  const p = norm(path);
  const r = norm(root);
  const abs = /^([A-Za-z]:\/|\/)/.test(p);
  if (!abs) return p.replace(/^\.\//, "");
  const ci = /^[A-Za-z]:\//.test(r);
  const pc = ci ? p.toLowerCase() : p;
  const rc = ci ? r.toLowerCase() : r;
  return pc === rc ? "" : pc.startsWith(rc + "/") ? p.slice(r.length + 1) : null;
}

// A path with a media extension, as written in prose, code or tool output.
const MEDIA_TOKEN = /(?:[A-Za-z]:[\\/]|~?[\\/]|\.{1,2}[\\/])?[\w@.+\-[\]()\\/]*\.(?:png|jpe?g|gif|webp|bmp|avif|svg|pdf|mp3|wav|ogg|oga|flac|m4a|aac|opus|mp4|m4v|webm|mov|ogv)\b/gi;

/** The media files a chat text mentions (code spans, links, bare paths), in order, without repeats. */
export function mediaPathsIn(text: string | null | undefined): string[] {
  if (!text) return [];
  const found: string[] = [];
  const add = (raw: string) => {
    const p = asPath(raw);
    if (p && mediaKind(p) && !found.includes(p)) found.push(p);
  };
  for (const m of text.matchAll(/\]\(([^)\s]+)\)/g)) {
    try {
      add(decodeURIComponent(m[1]));
    } catch {
      add(m[1]);
    }
  }
  for (const m of text.matchAll(MEDIA_TOKEN)) {
    // Skip URLs (https://host/a.png): only files are previewed.
    if (text[(m.index ?? 0) - 1] === ":") continue;
    // A markdown link's label and target run together ("[alt](a.png"): the link pass above has it.
    if (m[0].includes("](")) continue;
    add(m[0]);
  }
  return found;
}

/** The file a tool call is about (its path argument), for a link in the card's header. */
export function argPath(args: string): string | null {
  try {
    const o = JSON.parse(args);
    const v = o.path ?? o.file_path ?? o.filePath ?? o.file;
    return typeof v === "string" ? asPath(v) : null;
  } catch {
    return null;
  }
}

const NONE: string[] = [];
// Items are immutable (a changed item is a new object), so each is scanned once.
const scanned = new WeakMap<ChatItem, string[]>();

/**
 * The media files one transcript item mentions: an answer's or a thought's text, a finished tool
 * call's file argument and result, a "tool results" block's text. Nothing while a message is
 * still streaming (the path may be half written) or for a failed or running tool call.
 */
export function itemMedia(item: ChatItem): string[] {
  let found = scanned.get(item);
  if (found) return found;
  found = NONE;
  switch (item.kind) {
    case "assistant":
    case "thinking":
      if (!item.streaming) found = mediaPathsIn(item.text);
      break;
    case "system":
      found = mediaPathsIn(item.text);
      break;
    case "tool":
      if (item.done && !item.isError) {
        const path = argPath(item.args);
        found = [...(path && mediaKind(path) ? [path] : []), ...mediaPathsIn(item.result)].filter((p, i, a) => a.indexOf(p) === i);
      }
      break;
  }
  scanned.set(item, found);
  return found;
}

/** One file whichever way it is written (slashes, a leading "./", drive-letter case). */
const sameFile = (p: string) => {
  const n = p.replace(/\\/g, "/").replace(/^\.\//, "");
  return /^[A-Za-z]:\//.test(n) ? n.toLowerCase() : n;
};

/**
 * Where a turn's media is previewed without anything being expanded: each file ONCE per turn (a
 * turn runs from one user message to the next).
 *
 *  - Under the first finished answer of the turn that mentions it.
 *  - If no answer mentions it: right after the first tool call that produced or read it, so a
 *    generated image is never invisible (and shows while the turn is still running).
 *
 * Collapsible blocks (thoughts, tool cards, "tool results") show their own media only while
 * expanded; that is the block's own copy and isn't part of this plan. The result maps an item's
 * key to the files previewed under it.
 */
export function planMedia(items: readonly ChatItem[]): Map<number, string[]> {
  const plan = new Map<number, string[]>();
  let start = 0;
  const turn = (end: number) => {
    const placed = new Set<string>();
    const place = (kind: "assistant" | "tool") => {
      for (let i = start; i < end; i++) {
        const item = items[i];
        if (item.kind !== kind) continue;
        for (const p of itemMedia(item)) {
          const id = sameFile(p);
          if (placed.has(id)) continue;
          placed.add(id);
          const at = plan.get(item.key);
          if (at) at.push(p);
          else plan.set(item.key, [p]);
        }
      }
    };
    place("assistant");
    place("tool");
  };
  for (let i = 0; i < items.length; i++) {
    if (items[i].kind !== "user") continue;
    turn(i);
    start = i;
  }
  turn(items.length);
  return plan;
}
