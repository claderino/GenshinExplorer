# Genshin 7.x Protocol Notes

Everything reverse-engineered from live packet captures (Oct 2026, v7.x).
Command IDs are 7.x-specific and scrambled between game versions — the
*shapes* and *value patterns* are the durable knowledge. All detection in
this project is shape/value-based for exactly that reason.

Decryption: vendored `auto-artifactarium` (KCP + key exchange). Cold-start
logins after a game restart use a NEW encrypted key exchange (two RSA-2048
blobs, second unopenable) — relogs mid-session decrypt fine via
conversation re-adoption. See `keydump/` forensics if attacking that.

---

## 1. Player position

Two carriers, both in game world coordinates (x east, y up, z south):

| Carrier | Cadence | Notes |
|---------|---------|-------|
| direct (cmd 26016 in 7.x) | ~6 s | `{... 58 <float> ...}` — trajectory-confirmed ground truth |
| batch (cmd 2246 in 7.x) | ~1 Hz | Many entities; filter by the player's learned entity_id |

Continuity filter (`explore::PositionTracker`): accept jumps ≤ 50 units,
gap ≥ 0.5 s, > 15 s apart = teleport. `pos_tracker.last` is the
authoritative position used by all event matching.

**World → map transform** (map px = HoYoLab canvas pixels):

```
map_x = origin_x − world_z + cal_x
map_y = origin_y − world_x + cal_y
```

Axes swapped and negated; `origin` is the world-zero point in canvas px
(per map, from `detail_v2`); `cal` is the user calibration offset (per map,
persisted in `map_calibration_{map_id}.json`). Inverse (map → world):
`world_x = cal_y − pin_y`, `world_z = cal_x − pin_x` (origin cancels for
origin-relative pins).

---

## 2. Mora / item rewards

`ItemAddNotify`-shaped messages: top-level repeated bytes submessages
`{1: item_id, 2: count}` (ItemParam) or `{1: item_id, 5: {1: count}}`
(full Item), plus a top-level ActionReason varint.

- **Mora item id: 202.** Prop 10016 carries the balance.
- `reasons.rs` has the full ActionReasonType table (Sorapointa protos).
- Chest-family reasons: **39 OPEN_CHEST, 52 OPEN_WORLD_BOSS_CHEST,
  55 OPEN_BLOSSOM_CHEST**.
- **Chest open ⇒ ItemAdd with a chest-family reason fires** — including
  Mora-less chests (items granted, count 0 mora). This is the
  *authoritative collection signal*.
- The same command fires a per-item "652-shape" companion (below) at the
  same instant.

## 3. Gadget state broadcasts (NOT collection signals)

Two 7.x shapes fire for gadget state changes — **on unlock/approach/spawn,
not just collection**. Do not use for chest detection (a camp chest
unlocking during combat carries `{11: 39, 13: {202, amount}}` and looks
exactly like a reward):

- **652-shape**: `{11: action_reason, 13: {1: id, 2: count}}`
  e.g. chest contents preview `{11: 39, 13: {1: 202, 2: 780}}`;
  oculus collected-state `{11: 11, 13: {1: 107001, 2: 1, 4: guid}}`
- **25131-shape**: `{14: Vector{1: fx, 2: fy, 3: fz}, 15: <raw varint id>}`
  — the gadget's world position + item/config id. Chest → 202, oculi →
  107xxx, talk/other gadgets → 2xxx.

All such packets are logged to `gadgetrewards.jsonl` (uncapped) for future
signature mining (seelies, puzzles, torches…).

## 4. Oculus collection (verified reliable)

Oculus gadget **config ids 107001–107008** (Anemo, Geo, Electro, Dendro,
Hydro, Pyro, Cryo, Luno) appear as 3-byte varints in small (≤64 B)
commands exactly when collected — not on approach (validated by user over
multiple sessions). Byte patterns: `f9c306`…`ffc306`, `80c406`.

**Rule**: pattern seen + player within 45 map px of an uncollected
oculus pin ⇒ collected.

## 5. Challenge lifecycle (partially mapped)

Timed combat/archery challenges (7.x):

| Phase | Packets |
|-------|---------|
| start (F-press) | burst: 22443 `{3: type=177, 4: entity}`, 7266 `{..., 12: goal, 13: goal}`, 29437, 24935, 22292 `{3: entity, 5: 1, 7: 201}`, 7515 |
| **fail** (timer out) | **20234 `{3: seconds_used, 4: goal, 10: 1}`** + 22292 short + 799 |
| **success** | **20234 `{3: seconds_used, 4: goal, 7: 1, 10: 2}`** + 22292 `{..., 7: 202}` (no 799) |

- **20234 field 10 discriminates: 1 = failed, 2 = completed.** Field 3 is
  the timer (fail: full duration; success: time used). Verified on combat
  and archery timed challenges. Time trials (ring-collection) have NOT
  been observed emitting 20234 — they mark via the chest + dwell rule.
- Warrior's/combat-started challenges have no interact request at all
  (no 6516); time-trial starts produce 6516 bursts (client retries).
- `detect_interact`'s gadget registry misses most challenge gadgets —
  interact logging is therefore unreliable as an anchor.

### Matching design (why it is the way it is)

1. **Chest event (ItemAdd, chest reason)** → nearest uncollected pin of
   HoYoLab group 13 (true chest types) within 60 px, type-word preference.
2. **Dwell rule**: if no chest pin matched and the player stood ≥ 6 s
   within 40 px of an uncollected challenge pin (8 s–5 min before) with
   the chest event within 250 px ⇒ mark the challenge (spawned reward
   chest sits far from the start pin, sometimes near unrelated chest pins).
3. **Instant challenge**: 20234 success ⇒ mark challenge pin within 80 px
   of the player.
4. **Spawned-chest suppression**: chest events within 250 px of a
   challenge completed in the last 10 min are skipped (they are the
   spawned reward — must not mark unrelated chest pins).

## 6. UID detection

7.x removed user_id from wire headers. Avatar-vote: every SceneAvatarInfo
submessage `{1: uid (9 digits), 2: avatar_id (10xxxxxxx)}` votes; winner
at ≥ 3 votes = account UID. Namespaces all per-UI state (completion
buckets, pin filters).

## 7. HoYoLab map APIs (international)

All under `sg-public-api-static.hoyolab.com/common/map_user/ys_obc/…`
(`lang=en-us`):

| Endpoint | Purpose |
|----------|---------|
| `v3/map/info?map_id=` | name, `detail` (legacy slices) + `detail_v2` (tile pyramid): `total_size`, `origin`, `map_version`, `min_zoom..max_zoom` |
| `v3/map/point/list?map_id=` | pins + `label_list` (with `parent_id`) |
| `v2/map/label/tree?map_id=` | sidebar group hierarchy (15 top groups) |

- **Tile pyramid (all maps incl. Teyvat)**:
  `act-webstatic.hoyoverse.com/map_manage/map/{map_id}/{version}/{x}_{y}_{P0|N2}.png`
  — 256 px Leaflet grid, zoom 0 = native (Teyvat canvas 36864×18432,
  includes post-2024 regions). Legacy slice URLs = April 2024 map, unused.
- **Pin coordinates are origin-relative** (negative-capable): canvas px =
  `pin + origin` (true for every map incl. Teyvat v2 canvas).
- Maps: 2 Teyvat, 7 Enkanomiya, 9 Chasm Underground, 34 Sea of Bygone
  Eras, 36 Ancient Sacred Mountain, 37 Temple of Space, 40 Frost Moon.

## 8. HoYoLab marks API (authenticated sync)

Base: `sg-public-api.hoyolab.com/common/map_user/ys_obc` (no `-static`).
Auth = browser Cookie header (`cookie_token_v2` + `account_id_v2` +
`ltoken_v2`/`ltuid_v2` + `account_mid_v2`) **plus `uid` + `region` query
params** on reads — without uid/region the API hides behind
`-2004 "Please log in first"`. Region from UID prefix: 6→os_usa,
7→os_eu, 8→os_asia, 9→os_cht.

| Endpoint | Body / params | Notes |
|----------|---------------|-------|
| GET `v1/map/point/mark_map_point_list` | `map_id, uid, region, app_sn, lang` | `data.list[].point_id` (+ label_id) |
| POST `v1/map/point/add_mark_map_point` | `{map_id, point_id, app_sn, lang}` | re-add is not an error |
| POST `v1/map/point/new_del_mark_map_point` | same | unmark one |
| POST `v1/map/point/batch_mark_map_point` | `{map_id, list: [{point_id, is_delete}], app_sn, lang}` | **one endpoint does mark+unmark**, chunks of 100 |
| POST `v1/map/point/del_mark_point_by_label` | `{map_id, label_id, …}` | bulk per label — unused (batch is safer) |
| GET `v1/map/get_game_exploration_rate` | `map_id, uid, region` | region exploration % + game_role |

**Writes REQUIRE the x-rpc device headers** (`x-rpc-device_id`,
`x-rpc-device_fp`, `x-rpc-platform: 4`, `x-rpc-page`, `x-rpc-view_source`,
`x-rpc-map_version: 4.5`) — without them: `-502 "Something went wrong"`.
Device values parse from the cookie itself (`_HYVUUID`, `DEVICEFP`).

Other endpoints seen in the web app: `get_point_changelog` (pin db
changes per patch), `spot/*` + `spot_kind/*` (custom user markers),
`route_path` family (community routes), `ranking/list`, `shunt`
(returned `SS_UNAVAILABLE` — marks still work regardless).

## 10. Layered maps (point_group API + calibration frames)

Sub-areas with vertical layers (Frost Moon's dungeons, dark side, etc.):

**API**: `v2/map/point_group?map_id=` → groups → floors, each with:
- `overlay.url` + rect (l_x/l_y/r_x/r_y in **raw pin space**)
- `point_ids` (pins shown on that floor)
- `entrance_ids` (elevator/entrance pins)
- `vertical_space` (0 or 1 — capture station is 1)
- `underground_entrance_x/y` (surface canvas position of the group entrance)

**Rendering**: floor overlay PNGs drawn at rect + origin on the canvas;
pins filtered by floor ownership (surface shows only non-floor pins).

**Calibration frames**: separate world→canvas transforms per coordinate
space. All frames share one global scale (a property of the canvas);
offsets are per-frame. A single point anchors a frame.

**Scene discrimination**: scene_gen counter bumps on enter-scene packets
(>3 KB). Points remember their generation; same-gen frames get priority
in assignment. Any >200-unit position jump resets the continuity filter.

**Floor auto-selection** (priority order):
1. Teleport pin-match: nearest waypoint pin (label 3) within 120px →
   floor ownership via point_ids → **locks** the floor
2. Cross-scene entrance-match: pre-entry surface position → nearest
   floor-owned pin within 200px → locks
3. Rect containment (fallback, only when not locked): smallest containing
   overlay rect

**Position bounds**: ±100,000 on all axes — instanced activities exist at
y≈10,000 (capture station), z≈6,500+ (dark side), etc. The 26016 shape
match handles noise filtering.

**Known issues**:
- Overlapping floor rects can cause wrong rect-containment picks when
  pin-match misses (Moonheart Chasm 1377×1117px contains Darkside Base
  295×267px)
- Calibration dedup guard needed: reject points <30 units apart with
  clicks >50px apart (prevents inconsistent offsets)
- Floors sharing surface world coords (capture station interior if it
  weren't instanced) don't need separate calibration — surface transform
  covers them

## 11. Logging (research infrastructure)

| File | Content |
|------|---------|
| `smallcmds.jsonl` | all commands ≤ 128 B (cap 200k/session) with hex |
| `gadgetrewards.jsonl` | every 652/25131-shaped packet (uncapped) |
| `positions.jsonl` | player position ~2 s cadence |
| `chest_candidates.jsonl` | chest events + mora-correlation dumps |
| `census.jsonl` | per-10 s top-40 command histogram + first-seen hex samples |
| `interacts.jsonl` | registry-matched gadget interacts (unreliable) |

`map_calibration_{map_id}.json` — per-map world→map calibration.
`completed_pins.json` — `{"{uid}_{map}": [pin ids]}`.
`pin_filters_v3[_{uid}]_{map}.json` — enabled labels + hide-collected.
