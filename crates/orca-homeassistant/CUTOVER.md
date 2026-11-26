# Home Assistant cutover: ha-bridge → Abhoth Supervisor

This is part of the supervisor switchover (SUPERVISOR.md 11.3, SV16). It takes about
10 minutes. The rollback takes about 2 minutes.

Placeholders:

| Placeholder | Meaning |
|---|---|
| `<server>` | the server id in topics and unique ids, e.g. `deepthought` |
| `<app>` | the TrueNAS app that runs ha-bridge, e.g. `ha-bridge` |
| `<ha>` | the Home Assistant URL, e.g. `http://ha.lan:8123` |
| `<token-file>` | a file holding an HA long-lived token (never pasted into a shell) |
| `<mqtt-host>` | the broker host |

## What changes

- ha-bridge stops. The supervisor publishes HA discovery for the server, every card, the
  fans and the engines.
- With `legacy_ids = true`, these entities keep ha-bridge's unique ids, entity ids and
  history: GPU Temp, HBM Temp, GPU Power, SM Clock, GPU Utilization and VRAM Used (now on
  the **card B** device), Tokens per Second, Model, LLM Profile, LLM Card A/B, LLM Profile
  Switch, Bench Queue Running, LLM Profile To Apply and Switch To Selected Profile.
- **GPU Max Clock is retired.** Its discovery config is cleared, and HA removes the entity.
  Each card that allows a cap gets its own **Clock cap** select (`none` or 1200-1500 MHz,
  1 h TTL). Card A has none (`allow_cap = false`).
  - Automations that use the old GPU Max Clock select must be repointed to a card's Clock
    cap, or removed.
- With `clear_legacy_topics = true`, ha-bridge's retained topics are cleared at startup:
  - `deepthought/state`, `deepthought/bridge/availability`;
  - `deepthought/llm/state`, `deepthought/llm/target`;
  - `deepthought/settings/maxclock`;
  - the `deepthought/set/*` and `deepthought/cmd/*` command topics.
- HA controls now act as the MQTT service principal `mqtt:<server>`. They are audited and
  refused by the same guards and calendar as any other caller. HA cannot force anything.

## Before

1. Make sure no layout switch is running and no bench is queued.
2. Check that the supervisor config has the `[mqtt]` table, with credentials only as
   references:

   ```toml
   [mqtt]
   enabled = true
   url = "mqtt://<mqtt-host>:1883"
   client_id = "abhoth-supervisor-<server>"
   username = { file = "/run/secrets/mqtt-user" }
   password = { file = "/run/secrets/mqtt-password" }
   legacy_ids = true            # only once ha-bridge is stopped (step 2)
   clear_legacy_topics = true
   legacy_card = "B"            # the card ha-bridge's GPU sensors described
   ```

3. Run `abhoth supervisor config check`. It must report no `mqtt.` problems.
4. Note the live values of two HA sensors (for example GPU Temp and LLM Profile), to compare
   in the smoke check.

## Cutover

1. **Stop ha-bridge:** `midclt call -job app.stop <app>`.
   The broker publishes its last will, so the old entities go `unavailable`. That is
   expected.
2. **Start or reload the supervisor** with the `[mqtt]` table above. Do this only after
   step 1: two publishers with the same unique ids fight over the entities.
3. Wait 30 s. The supervisor:
   - publishes `online` on `abhoth/<server>/availability`;
   - clears the legacy topics and the GPU Max Clock config;
   - publishes discovery and state;
   - sweeps any `abhoth_<server>_*` configs that a previous run left behind.

## Smoke check (5 minutes)

All of these are read-only, apart from one no-op command.

1. In HA, open **Settings → Devices**:
   - The **DeepThought** device now shows the server entities.
   - **DeepThought card A** to **card D** exist, each with its sensors.
   - **GPU Max Clock** is gone.
2. The old entity ids still work, and their history continues with no gap longer than the
   cutover: GPU Temp now shows card B, and LLM Profile shows the live layout.
3. Spot-check one value against the host. Card B core temperature should match
   `nvidia-smi --query-gpu=temperature.gpu --format=csv` for that card, within a degree.
4. Read a state through the REST API without echoing the token:

   ```
   curl -s -H "Authorization: Bearer $(cat <token-file>)" <ha>/api/states/<a legacy entity id>
   ```

5. Send one no-op command: set card B's **Clock cap** to its current value (usually
   `none`).
   - In the supervisor log, it appears as an HA command from `mqtt:<server>` and is
     accepted.
   - Nothing else happens.
6. Check that the supervisor log shows no repeated `mqtt` connect errors.

If any check fails, roll back.

## Rollback

1. **Stop the supervisor's HA publisher.** Set `[mqtt] enabled = false` and reload, or stop
   the supervisor. Its entities go `unavailable` (orderly `offline`, or the last will).
2. **Start ha-bridge:** `midclt call -job app.start <app>`.
   - It republishes its 16 discovery configs on the same topics. That takes back the
     legacy unique ids and brings GPU Max Clock back.
   - It republishes its state and availability topics.
   - The GPU Max Clock state comes back as its last retained value. It was reset to
     `No limit` on 2026-11-26.
3. **Optional tidy-up.** The `abhoth_<server>_*` entities stay `unavailable` until they are
   removed. Either:
   - delete the **DeepThought card A-D**, fan and engine devices in HA; or
   - clear their retained configs with an empty retained publish to each
     `homeassistant/<component>/abhoth_<server>_*/config` topic.

   Leaving them in place is harmless. A later cutover reuses them.
