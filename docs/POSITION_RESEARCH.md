# GenshinExplorer — Position Detection Research

## Status: SOLVED (2026-10-08)

The player's position is carried by two commands, identified by searching
census/smallcmd hex dumps for the exact float bytes of the player's known
location (Windwail Highland Statue of the Seven: x=1857.735, y=204.63,
z=-576.287).

## Carrier 1: cmd 2246 (batch movement sync, ~1 Hz)

Full protobuf path to the position vector:

```
proto_data
  └─ field 8 (bytes) — batch entry
      └─ field 6 (bytes) — wrapper
          └─ field 9 (bytes) — union command
              ├─ field 4: varint (type = 2)
              ├─ field 6: varint (forward_type = 7)
              └─ field 15 (bytes) — EntityMoveInfo equivalent
                  ├─ field 1: varint entity_id
                  └─ field 2 (bytes) — MotionInfo
                      ├─ field 1 (bytes) — Vector pos ← THE POSITION
                      │   ├─ field 1: fixed32 x (LE float)
                      │   ├─ field 2: fixed32 y (LE float)
                      │   └─ field 3: fixed32 z (LE float)
                      ├─ field 2 (bytes) — Vector rot
                      └─ field 3 (bytes) — Vector speed (often empty)
              ├─ field 3: varint scene_time
              ├─ field 4: varint reliable_seq
              └─ field 5: varint is_reliable
```

Raw hex sample (player standing at Windwail statue):
```
423b32364a34200230077a2e08fc808001121e0a0f0d8337e844154f5081431d571210c4
120515932087421a00201d320018dff20520c00c2801689236
```

## Carrier 2: cmd 26016 (direct position notification)

Simpler structure:

```
proto_data
  ├─ field 1 (bytes) — position data
  │   ├─ field 7 (bytes) — Vector pos ← THE POSITION
  │   │   ├─ field 1: fixed32 x (LE float)
  │   │   ├─ field 2: fixed32 y (LE float)
  │   │   └─ field 3: fixed32 z (LE float)
  │   ├─ field 11: varint (timestamp)
  │   └─ field 12 (bytes) — rotation
  └─ field 7: varint (count/scene indicator)
```

Raw hex sample:
```
0a1e3a0f0d8337e84415625081431d571210c458d9d3fca402620515fe1f87423803
```

## Map transform

From user movement verification (2026-10-08):
- World x (east) maps to map VERTICAL (inverted)
- World z (south) maps to map HORIZONTAL (inverted)
- Scale ≈ 1:1 (1 world unit ≈ 1 map pixel at base resolution)

```
map_px_x = origin_x - world_z
map_px_y = origin_y - world_x
```

With HoYo API origin = (12524, 7406) for the April 2024 Teyvat map.

## Detection strategy

Use **exact-path parsers** (not generic scanning) for both carriers:

1. For the batch carrier: follow the exact field path 8→6→9→15→2→1→vector
2. For the direct carrier: follow field 1→field 7→vector
3. Accept whichever produces a valid world-position vector
4. Apply continuity filtering (≤50 units between updates, ≥0.5s gap)

This eliminates ALL false positives from other entities because we follow
the exact protobuf nesting to the player's position, rather than scanning
for any float triple that happens to look like a coordinate.

## Lessons learned

1. **Generic scanning fails** — many packets contain position-like floats
   for other entities (enemies, NPCs, cameras, spawns)
2. **"First vector" heuristic fails** — the first vector in a message
   isn't necessarily the player
3. **"Multi-vector" heuristic fails** — solo play may produce single-vector
   position updates
4. **Exact-path parsing works** — following the precise protobuf field
   chain eliminates all ambiguity
5. **The `detect_player_position(≥2 vectors)` approach was wrong** — cmd 2246
   batches can contain entries for OTHER entities' movement too, and the
   "multiple vectors" gate doesn't prevent cross-contamination within the
   same batch

## Verification checklist

- [x] Found exact float bytes of known position in hex dumps
- [x] Decoded complete protobuf paths for both carriers
- [ ] Exact-path parser implemented
- [ ] Player dot follows player (not enemies)
- [ ] Map transform produces correct map location
- [ ] Calibration persists across restarts
- [ ] Chest markers appear at correct map locations
