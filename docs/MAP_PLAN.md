# GenshinExplorer — Map Enhancement Plan

> **Status (Oct 2026):** Phases complete — docked map, v2 tile pyramid
> (sharp zoom), all sub-maps, HoYoLab label-tree categories, icon pins,
> per-UID completion, auto-collect (chests/oculi/challenges via packets).
> Remaining: Phase 2 (HoYoLab sync). Packet knowledge: `PROTOCOL_NOTES.md`.

## Goal
Transform the basic map window into a full interactive map with all
88K HoYoLab pins, category filtering, sub-map support, and per-account
completion tracking — powered by our packet-based position detection.

---

## Data Sources

### International API (English, accessible from user's network)

| Resource | Endpoint |
|---|---|
| Map info (tiles, origin) | `sg-public-api-static.hoyolab.com/common/map_user/ys_obc/v3/map/info?map_id=X&app_sn=ys_obc&lang=en-us` |
| Pin data (88K+ points) | `sg-public-api-static.hoyolab.com/common/map_user/ys_obc/v3/map/point/list?map_id=X&app_sn=ys_obc&lang=en-us` |
| Tile CDN | `act-webstatic.hoyoverse.com/map_manage/...` |

### Available sub-maps

| Map ID | Name | Notes |
|---|---|---|
| 2 | Teyvat (main) | April 2024 tiles, 10×10 grid |
| 7 | Enkanomiya | Single 4096px tile |
| 9 | Chasm Underground Mines | Single 4096px tile |
| 34 | Sea of Bygone Eras | 2×1 tiles |
| 36 | Ancient Sacred Mountain | Single 4096px tile |
| 37 | Temple of the Sky | Single tile |
| 40 | Frostmoon | Single large tile |

### Pin data structure

Each pin: `{ id, label_id, x_pos, y_pos, area_id, z_level }`
- `x_pos`, `y_pos` are in **map pixel coordinates** (same space as tiles)
- No transform needed to render on the map image
- 706 label types in a hierarchy (`parent_id` → categories → sub-labels)

### Key label categories (by gameplay relevance)

| Category | Label IDs | Pin count (est.) |
|---|---|---|
| Chests (all types) | 17, 44, 45, 46, 269, + regional variants | ~5,000 |
| Oculi (all elements) | 5, 6, 194, 403, 508, 626, 833, 694 | ~1,200 |
| Teleport Waypoints | 3 | ~300 |
| Statues of the Seven | 2 | ~50 |
| Domains | 154 | ~100 |
| Shrines of Depths | 6 regional variants | ~30 |
| World Quests | 52 | ~200 |
| Seelies | 18, 148, 205 | ~1,000 |
| Puzzles | 71, 77, 231, 369 | ~2,000 |
| Materials/Enemies | 60+ labels | ~70,000 |

---

## Architecture

### File layout

```
%LOCALAPPDATA%\GenshinExplorer\
  map\
    info_2.json            # metadata per map_id
    tiles_2\               # cached tile PNGs
    pins_2.jsonl           # pin data (compact, one per line)
    labels_2.json          # label hierarchy
    info_7.json            # ... (same pattern for each sub-map)
    tiles_7\
    pins_7.jsonl
  map_calibration.json     # per-map calibration offsets
  pin_completion.json      # { map_id: { uid: [completed_point_ids] } }
```

### Spatial indexing

88K pins is too many to iterate every frame. Build a simple **grid bucket
index** at load time:

```rust
struct PinIndex {
    cell_size: f64,                    // e.g., 256 pixels
    cells: HashMap<(i32, i32), Vec<usize>>,  // (col, row) → pin indices
    pins: Vec<Pin>,                    // flat array
}
impl PinIndex {
    fn query(&self, bbox: &Rect) -> Vec<&Pin> { ... }
}
```

At typical zoom levels, a viewport covers ~10-100 grid cells containing
~50-500 pins. Rendering 500 dots per frame is trivial.

At low zoom (whole map visible), cap rendering at ~2,000 pins by:
- Showing only "important" categories (chests, oculi, waypoints)
- Skipping materials/enemies (they're only useful at high zoom)

---

## Rendering plan

### Approach (based on HoYoLab's Leaflet-based implementation)

HoYoLab uses **canvas rendering** with **viewport culling** and **zoom-dependent
clustering**. At low zoom, nearby pins are grouped into cluster badges
("5", "12"). As you zoom in, clusters split into individual markers.
Only pins in the visible viewport are rendered.

We'll use egui's `painter` API for the same effect:
- `painter.circle_filled()` / `painter.rect_filled()` for pin shapes
- GPU-accelerated, easily handles 2,000+ visible pins per frame
- Viewport culling via our spatial grid index (only query visible cells)

### Clustering (at low zoom)

When `zoom < 0.5`, group pins by grid cell:
- Count pins per cell (e.g., 128px cells)
- If cell has 1 pin → draw the pin normally
- If cell has 2+ pins → draw a single circle with the count as text
- This prevents 88K dots from covering the entire map at overview zoom

### Pin visual design

| Category | Marker | Color | Min zoom |
|---|---|---|---|
| Common Chest | circle | brown (#C97F32) | 0.3× |
| Exquisite Chest | circle | blue (#5096FF) | 0.3× |
| Precious Chest | circle | purple (#BE5ADC) | 0.3× |
| Luxurious Chest | circle | gold (#F0C83C) | 0.3× |
| Remarkable Chest | circle | orange (#FF8C42) | 0.3× |
| Oculi (all types) | diamond | per-element | 0.3× |
| Teleport Waypoint | diamond | blue (large) | 0.3× |
| Statue of Seven | diamond | gold (large) | 0.3× |
| Domain | square | purple | 0.5× |
| Shrine of Depths | square | gold | 0.5× |
| Seelie | circle | light blue | 0.8× |
| World Quests | circle | yellow | 0.5× |
| Puzzles | circle | cyan | 0.8× |
| Materials | dot (small) | green | 1.5× |
| Enemies | dot (small) | red | 2.0× |

- **Completed pins**: faded (30% opacity) + small check mark
- **Hovered pin**: enlarged 1.5× + tooltip: `name (category) — ✓ completed` or `— uncompleted`
- **Tooltip delay**: 200ms hover

### Icon images (Phase 2 enhancement)

The label API includes an `icon` URL per label type. We could download
these (~50 icons for key categories) and render as textured quads instead
of colored shapes. This would look identical to HoYoLab but adds texture
management complexity. V1: colored shapes. V2: icon images.

---

## Filter UI (matches HoYoLab behavior)

### Left sidebar with hierarchical checkboxes

```
🔍 [search pin name...          ]

▼ ✅ Chests                    (5,234 shown)
     ✅ Common Chest           (1,234)
     ✅ Exquisite Chest        (2,345)
     ✅ Precious Chest         (1,000)
     ✅ Luxurious Chest          (200)
     ✅ Remarkable Chest       (1,221)
     ☐ Natlan Mora Chest        (156)
     ☐ Fontaine Mora Chest      (300)

▼ ✅ Oculi                     (1,200 shown)
     ✅ Anemoculus               (66)
     ✅ Geoculus                 (131)
     ✅ Electroculus            (181)
     ✅ Dendroculus             (270)
     ✅ Hydroculus              (288)
     ✅ Pyroculus               (200)
     ✅ Cryoculus                66
     ☐ Lunoculus                 30

▼ ✅ Waypoints                   (300 shown)
     ✅ Teleport Waypoint       (250)
     ✅ Waverider Waypoint       (50)

▼ ✅ Domains                     (100 shown)
▼ ✅ Statues of the Seven         (52 shown)
▼ ✅ Shrines of Depths            (36 shown)
▼ ✅ Seelies                    (1,000 shown)
▼ ☐ World Quests                 (200 hidden)
▼ ☐ Puzzles                     (2,000 hidden)
▼ ☐ Materials                  (70,000 hidden)
▼ ☐ Enemies                     (15,000 hidden)

─────────────────────────────
☑ Show completed pins
[Sync with HoYoLab...]
☑ Auto-sync on detection
```

### Filter behavior (identical to HoYoLab)
- Top-level checkbox: toggles ALL sub-labels in that category
- Individual checkboxes: toggle specific pin types
- Indeterminate state (▬): some but not all sub-labels enabled
- Filter state **persists** across restarts (stored in config)
- Search box: filters the label tree by name, highlights matches
- Pin count shown per category AND per sub-label
- "Show completed" master toggle: hide/show already-completed pins

### Filter state storage

```json
{
  "enabled_labels": {
    "17": true,     // Common Chest
    "44": true,     // Exquisite Chest
    "45": true,     // Precious Chest
    "46": true,     // Luxurious Chest
    "269": true,    // Remarkable Chest
    "5": true,      // Anemoculus
    ...
  },
  "show_completed": true,
  "auto_sync": false
}
```

---

## Sub-map switching

### Scene detection

Map the game's `scene_id` (from packets) to `map_id`:

| Scene ID | Map ID | Region |
|---|---|---|
| 3 | 2 | Teyvat (overworld) |
| 5 | 7 | Enkanomiya |
| 6 | 9 | Chasm Underground |
| ? | 34 | Sea of Bygone Eras |
| ? | 36 | Ancient Sacred Mountain |
| ? | 37 | Temple of the Sky |
| ? | 40 | Frostmoon |

Unknown scene IDs: learn empirically (when position tracking detects a
teleport, check which sub-map's coordinate range the new position falls
in) or find from community datamining.

### Auto-switching

When the player teleports to a different scene:
1. Detect the scene change (position jump + 15s gap in continuity filter)
2. Look up the new scene_id → map_id
3. Switch the map display to the new map
4. Load that map's tiles + pins if not already cached
5. Reset the position transform for the new map's origin

### Per-map calibration

Each sub-map has a different `origin` from the API. Store calibration
offsets per map_id:

```json
{
  "2": { "offset_x": 0, "offset_y": 0 },
  "7": { "offset_x": 0, "offset_y": 0 }
}
```

---

## Completion tracking

### Data model

```json
{
  "2": {
    "693185688": {
      "105915": { "ts": "2026-10-08T14:46:46Z", "source": "auto" },
      "106001": { "ts": "2026-10-08T14:47:12Z", "source": "auto" }
    }
  },
  "7": { ... }
}
```

Keyed by: `map_id → uid → point_id → {timestamp, source}`

### Auto-completion from packet detection

When we detect a chest open (Mora gain with reason 39/OPEN_CHEST):

1. Get player's world position from PositionTracker
2. Convert to map pixel coordinates
3. Query the pin index for the nearest uncompleted chest within 50 pixels
4. If found, mark as completed with `source: "auto"`
5. Log the association for confidence tracking

### Manual completion

- Right-click a pin → toggle completed/uncompleted
- Shift+click → complete all pins of the same type within 100px radius
  (bulk-complete a cluster of chests you just cleared)

---

## HoYoLab sync (Phase 2)

### Authentication (auto-extraction + fallback)

The HoYoLab interactive map requires browser login cookies (`ltoken`, `ltuid`).
Instead of making the user manually copy these every time they expire, we'll
try three extraction methods in order:

#### Method 1: Browser cookie database (primary — automatic)

Most users are logged into HoYoLab in Chrome/Edge. Their cookies are stored
in an encrypted SQLite database:

```
Chrome:  %LOCALAPPDATA%\Google\Chrome\User Data\Default\Network\Cookies
Edge:    %LOCALAPPDATA%\Microsoft\Edge\User Data\Default\Network\Cookies
```

The cookies are encrypted with Windows DPAPI (tied to the user account).
Extraction process:
1. Copy the cookie DB to a temp file (Chrome locks it while running,
   but a snapshot copy works on NTFS)
2. Open as SQLite, query for cookies where `host_key LIKE '%hoyolab.com%'`
3. Decrypt the `encrypted_value` column using DPAPI (`CryptUnprotectData`)
4. Extract `ltoken`, `ltuid`, `account_id`, `cookie_token`

Rust implementation:
- `rusqlite` for SQLite access
- `windows` crate's DPAPI (`Win32_Security_Cryptography.CryptUnprotectData`)
- Chrome's newer cookie encryption uses AES-GCM with a key stored in
  `Local State` (also DPAPI-encrypted) — need to handle both v1 (raw DPAPI)
  and v10/v11 (AES-GCM) formats

This runs automatically on app start and whenever auth fails. If the user
has HoYoLab open in their browser, the cookies are always fresh.

#### Method 2: Game web cache monitoring (automatic, passive)

When the user opens any in-game web feature (events, Paimon menu → Feedback/
HoYoLab), the game's embedded browser creates cache entries at:

```
%APPDATA%\..\LocalLow\miHoYo\Genshin Impact\webCaches\<version>\Cache\...
```

These cache entries contain URLs and headers with auth tokens. We already
have file-watching infrastructure (irminsul's `wish.rs` uses this approach
for wish history authkeys).

We'll monitor this directory and extract any HoYoLab tokens that appear.

#### Method 3: Manual paste (fallback — last resort)

A simple text box where the user pastes their cookie string:
```
ltoken=xxx; ltuid=123456; account_id=xxx
```

Only shown when Methods 1 and 2 both fail. The UI explains how to get
the cookies (open act.hoyolab.com → F12 → Network tab → copy Cookie header).

### Auth lifecycle

```
App start
  ├─ Try browser cookie extraction
  │   ├─ Success → use cookies → done
  │   └─ Fail (browser not found / locked / not logged in)
  │       ├─ Try game web cache
  │       │   ├─ Success → use cookies → done
  │       │   └─ Fail (no in-game web features opened recently)
  │       │       └─ Show "Paste cookies" UI
  │       └─ ...
  └─ During gameplay:
      ├─ Auto-sync ON: if API returns 401/403 (expired)
      │   ├─ Retry browser extraction (cookies may have refreshed)
      │   ├─ Retry game web cache
      │   └─ Show "Auth expired — [Retry] [Paste manually]" notification
      └─ Auto-sync OFF: only sync when user clicks "Push to HoYoLab"
```

### Auto-sync toggle

In the filter sidebar (bottom section):

```
─────────────────────────────
☑ Show completed pins
[⚙ Sync with HoYoLab...]
☑ Auto-sync on detection
```

**When auto-sync is ON:**
- Every time packet detection confirms a chest open / oculi pickup /
  waypoint unlock, the nearest uncompleted pin is marked complete locally
  AND pushed to HoYoLab immediately
- If the push fails (network error, expired cookie), the completion is
  stored locally with `synced: false` and retried on next sync

**When auto-sync is OFF:**
- Completions are stored locally only
- A "Push to HoYoLab" button appears (batch-push all unsynced completions)
- Manual completions (right-click) are always local-only until explicitly
  pushed

**Sync status indicator:**
- 🟢 Synced — all completions pushed to HoYoLab
- 🟡 Pending — N completions awaiting push
- 🔴 Error — auth expired / network issue

### API endpoints (to research)

| Operation | Likely endpoint |
|---|---|
| Read completion status | `GET .../v1/map/point/status/list?map_id=X` |
| Mark pin complete | `POST .../v1/map/point/status/update` |
| Reset pin | `DELETE .../v1/map/point/status/update` |

All require auth headers built from the cookie values.

### Sync flow

1. On first setup: user pastes cookie string → stored locally
2. On map open: fetch completion status from HoYoLab
3. Merge with local completion state (HoYoLab wins for manual marks,
   local wins for auto-detected marks that haven't been synced yet)
4. Push local-only completions to HoYoLab (if auto-sync is on)
5. During gameplay: auto-completion events push to HoYoLab immediately
   (if auto-sync is on) or queue locally (if off)

---

## Implementation phases

### Phase 1a: Pin loading + rendering (~1 session)
- [ ] Pin download + cache from international API
- [ ] Label hierarchy loading + resolution
- [ ] Spatial grid index
- [ ] Pin rendering (viewport culling, category colors)
- [ ] Basic filter UI (top-level category checkboxes)
- [ ] Performance test with 88K pins

### Phase 1b: Sub-maps + switching (~0.5 session)
- [ ] Tile download for all map_ids
- [ ] Map selector UI
- [ ] Scene detection → auto-switch
- [ ] Per-map calibration storage

### Phase 1c: Completion tracking (~0.5 session)
- [ ] Completion state persistence
- [ ] Right-click toggle
- [ ] Auto-completion from chest detection events
- [ ] Visual distinction (faded/dimmed completed pins)

### Phase 2: HoYoLab sync (~1 session)
- [ ] Auth cookie input UI
- [ ] Read completion from HoYoLab
- [ ] Write updates to HoYoLab
- [ ] Two-way merge logic
- [ ] Auto-push on chest open

---

## Performance budget

| Operation | Target |
|---|---|
| Pin data load (from cache) | < 500ms |
| Spatial index build | < 100ms |
| Per-frame pin query + render | < 5ms (at ≤ 2K visible pins) |
| Sub-map switch (cached) | < 200ms |
| Sub-map switch (download) | ~30-60s (background, progress bar) |

---

## Risks & mitigations

| Risk | Mitigation |
|---|---|
| HoYoLab API changes | Pin data cached locally; graceful degradation |
| 88K pins slow rendering | Spatial culling; zoom-dependent filtering |
| Old map tiles (April 2024) | Sub-maps have newer tiles; note in UI |
| Auth cookie expiration | Clear error message + re-auth prompt |
| Pin position drift | Auto-completion uses 50px radius (generous) |
| Multiple accounts on same PC | Completion state keyed by UID |
