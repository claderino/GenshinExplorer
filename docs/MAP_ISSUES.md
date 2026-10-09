# Map Issues — Root Cause Analysis & Fix Plan

## Issue 1: Pins don't appear

**Root cause**: Coordinate system mismatch. The API pins use original map pixel
coordinates (0–22528 × 0–20480), but the viewport culling query and rendering
code may be using inconsistent conversions between original ↔ stitched
(8192px) ↔ screen coordinates. Additionally, the category filter defaults
may be hiding everything (categories with >5000 pins disabled, which includes
most chest types grouped under a parent that exceeds the threshold).

**Fix**: Add a debug overlay showing visible pin count + coordinate ranges.
Fix the leaf-vs-parent counting logic. Verify with a specific known pin.

## Issue 2: Old map tiles

**Root cause**: We're downloading tiles from the CN API (`api-static.mihoyo.com`)
which serves April 2024 tiles for map_id=2. The international API
(`sg-public-api-static.hoyolab.com`) has the same v1 tiles, but the **v3 API**
has updated tile URLs from `act-webstatic.hoyoverse.com`.

**Fix**: Switch all tile downloads to the international v3 API. The sub-maps
(ids 36, 37, 40) have newer tiles for Natlan, Temple of the Sky, Frostmoon.

## Issue 3: Pin categorization doesn't match HoYoLab

**Root cause**: I built the category tree from the raw `parent_id` hierarchy,
but HoYoLab's sidebar doesn't use the raw label tree — it uses a curated
grouping. The raw data has 706 labels in a messy hierarchy that doesn't match
the sidebar UI HoYoLab shows users.

**Fix**: Build a curated category mapping (like HoYoLab's sidebar) instead of
deriving from raw parent_ids. Group pins into user-facing categories:
Chests → Common/Exquisite/Precious/Luxurious/Remarkable
Oculi → per-element
Waypoints → Teleport/Waverider
Domains, Shrines, Seelies, Materials, Enemies, etc.

## Issue 4: No zoom layers or sub-map switching

**Root cause**: We only support map_id=2 (main Teyvat) with a single
stitched resolution. HoYoLab serves tiles at multiple zoom levels and has
separate sub-maps for instanced regions.

**Fix**: 
- Download tiles for all sub-maps (7, 9, 36, 37, 40)
- Add a map selector UI (like HoYoLab's region selector)
- Auto-switch based on scene_id from packet detection
- Use higher-resolution tiles (the API serves 4096px tiles at max zoom)
- Implement zoom-dependent tile resolution

## Issue 5: Colored dots instead of icons

**Root cause**: I used simple colored circles for performance. But the HoYoLab
API provides icon URLs for each label type.

**Fix**: 
- Download label icons (~50 unique icons for key categories)
- Load as textures
- Render as small textured quads instead of colored circles
- Scale with zoom level

## Proposed fix order

1. **Fix pins appearing** (debug coordinate conversion, fix filter defaults)
2. **Switch to international v3 API for tiles** (newer tiles, English)
3. **Curated category sidebar** (match HoYoLab's grouping)
4. **Sub-map support + map switching** (all map IDs, auto-switch)
5. **Icon rendering** (download and use label icons)

All five are interconnected — the map source change (2) affects sub-maps (4),
which affects pin rendering (1), which affects categorization (3) and icons (5).
A coordinated rewrite of the map module is more effective than piecemeal fixes.

## Estimated effort
- Items 1-3: One session
- Items 4-5: One session
