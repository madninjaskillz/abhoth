# Mascot moods (D150)

User request (2026-11-26): six expression variants of the mascot, "could we use them on
responses based on the outcome?"

## The faces

`client/src/assets/brand/moods/abhoth-mood-<mood>-<64|128|256>.webp`: happy, sad, angry,
surprised, wink, sleepy. Cut out of the artist's 1254x1254 PNGs (opaque black background; the
outline is a dark purple, so the alpha comes from the outline's edge) and framed exactly like the
neutral mascot (`abhoth-mascot-256.webp`: same scale and position), so a change of face doesn't
move the character. `tools/brand/cut_moods.py` makes them.

## When each face shows

The mood comes only from structured turn state, never from the text of a message:

- the `turnEnd` event's `outcome` (`done`, `error`, `cancelled`) and `reason`
  (`orca_core::live::reasons`: `auth` for HTTP 401/403 and a router that refused the key,
  `rateLimited` 429, `quota` 402, `noModel`, `unavailable` for an engine that cannot run,
  `server` for other model-server failures, `failed` for a failed engine turn,
  `iterationLimit`);
- the turn's permission answers (`permissionResolved` with `no`);
- the levels of the turn's notices (`error`, `warning`: compaction, context nearly full, retries);
- questions and permission prompts still waiting.

One table, `TURN_MOODS` in `client/src/lib/moods.ts`; the first match wins:

| Face | When |
|---|---|
| none | you stopped the turn |
| angry | blocked: key refused (401/403), rate limited (429), out of quota (402), no model, engine unavailable, or a permission was denied |
| sad | failed: a model or server error, the engine failed, the iteration limit, or an error notice |
| surprised | it needs you (a question or permission still waiting at the end) or a warning came up |
| wink | fixed: it finished right after a turn that was sad or angry (only the first success after, so it stays rare) |
| happy | the turn finished |

While a turn runs, the small mascot beside "working…" is surprised while a question or
permission waits for you and sleepy once the turn has run for two minutes; otherwise it is the
neutral mascot. The empty chat ("type a prompt to start") shows the sleepy face.

## Where

- At the end of each response: a 28 px face in the turn's footer line (the time and token count),
  with the rule in its tooltip.
- The "working…" line and the empty chat, as above.
- Branding spots keep the neutral mascot: the title bar, sign-in and setup, the add-project dialog.

A face fades in over 0.4 s when it changes, and not at all with reduced motion. The **Moods**
checkbox under the composer turns them off (on by default, remembered in the browser).

## Cache busting

The brand images are bundled by Vite (imported from `src/assets/brand`, never inlined), so they
are served as `/assets/abhoth-<name>-<hash>.<ext>` and cached for good. The OpenOrca build served
its orca at `/brand/mascot-256.webp` and friends, and browsers kept showing those cached copies
after the rebrand; no path the old build used holds an image now. `/brand/branding.json` stays
where it is (never cached) for hosts that serve their own.
