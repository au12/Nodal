//! CPU side of the virtual texture: atlas slot allocation (LRU) and the page table.
//!
//! The page table has one entry per tile position for every detail level (≥ 1). An entry
//! always points at the *best resident tile covering that position* — the tile itself if
//! it is resident, otherwise its nearest resident ancestor — so the shader resolves any
//! sample with a single lookup. Entries with no resident ancestor fall back to the base
//! layer (level 0), which is a separate, always-resident mipmapped texture.
//!
//! Invariant: `entry(L, p) = own slot if resident(L, p) else entry(L-1, parent(p))`, with
//! `entry(0, _) = 0` (base). Insertions and evictions recompute only the affected subtree.

use nodal_core::pyramid::TileId;
use std::collections::HashMap;

pub const VALID: u32 = 1 << 31;

#[inline]
pub fn encode_entry(slot: u32, level: u32) -> u32 {
    VALID | (level << 24) | (slot & 0x00FF_FFFF)
}

#[inline]
pub fn entry_level(e: u32) -> Option<u32> {
    (e & VALID != 0).then_some((e >> 24) & 0x7F)
}

#[derive(Clone, Copy, Debug)]
struct Slot {
    tile: TileId,
    last_used: u64,
}

pub struct TileCache {
    pub levels: u32,
    pub slot_count: u32,
    slots: Vec<Option<Slot>>,
    free: Vec<u32>,
    resident: HashMap<TileId, u32>,
    /// `page[face * (levels-1) + (level-1)]` holds `2^level × 2^level` entries.
    page: Vec<Vec<u32>>,
    dirty: Vec<bool>,
    frame: u64,
}

impl TileCache {
    pub fn new(levels: u32, slot_count: u32) -> Self {
        let detail = levels.saturating_sub(1) as usize;
        let mut page = Vec::with_capacity(6 * detail);
        for _face in 0..6 {
            for level in 1..levels {
                let n = 1usize << level;
                page.push(vec![0u32; n * n]);
            }
        }
        Self {
            levels,
            slot_count,
            slots: vec![None; slot_count as usize],
            free: (0..slot_count).rev().collect(),
            resident: HashMap::new(),
            dirty: vec![false; page.len()],
            page,
            frame: 1,
        }
    }

    #[inline]
    fn page_index(&self, face: u8, level: u8) -> usize {
        face as usize * (self.levels as usize - 1) + (level as usize - 1)
    }

    pub fn begin_frame(&mut self) {
        self.frame += 1;
    }

    pub fn is_resident(&self, t: &TileId) -> bool {
        self.resident.contains_key(t)
    }

    pub fn resident_count(&self) -> usize {
        self.resident.len()
    }

    /// Marks a resident tile as used this frame (protects it from eviction).
    pub fn touch(&mut self, t: &TileId) {
        if let Some(&s) = self.resident.get(t) {
            if let Some(slot) = &mut self.slots[s as usize] {
                slot.last_used = self.frame;
            }
        }
    }

    /// Reserves a slot for `t`, evicting the least recently used tile that was not touched
    /// this frame. Returns `None` when every slot is in use by visible tiles.
    pub fn allocate(&mut self, t: TileId) -> Option<u32> {
        debug_assert!(t.level >= 1);
        if let Some(&s) = self.resident.get(&t) {
            return Some(s);
        }
        let slot = match self.free.pop() {
            Some(s) => s,
            None => {
                let (idx, victim) = self
                    .slots
                    .iter()
                    .enumerate()
                    .filter_map(|(i, s)| s.map(|s| (i, s)))
                    .filter(|(_, s)| s.last_used < self.frame)
                    .min_by_key(|(_, s)| (s.last_used, std::cmp::Reverse(s.tile.level)))?;
                self.evict(victim.tile);
                let _ = idx;
                self.free.pop()?
            }
        };
        self.slots[slot as usize] = Some(Slot { tile: t, last_used: self.frame });
        self.resident.insert(t, slot);
        self.update_subtree(t);
        Some(slot)
    }

    fn evict(&mut self, t: TileId) {
        if let Some(slot) = self.resident.remove(&t) {
            self.slots[slot as usize] = None;
            self.free.push(slot);
            self.update_subtree(t);
        }
    }

    /// Drops every detail tile (e.g. when the atlas is lost).
    pub fn clear(&mut self) {
        let tiles: Vec<_> = self.resident.keys().copied().collect();
        for t in tiles {
            self.evict(t);
        }
    }

    fn own_or_parent(&self, t: TileId) -> u32 {
        if let Some(&s) = self.resident.get(&t) {
            return encode_entry(s, t.level as u32);
        }
        match t.parent() {
            Some(p) if p.level >= 1 => self.page[self.page_index(p.face, p.level)][p.y as usize * (1 << p.level) + p.x as usize],
            _ => 0,
        }
    }

    /// Re-derives entries for `t` and all its descendants (top-down).
    fn update_subtree(&mut self, t: TileId) {
        for level in t.level as u32..self.levels {
            let span = 1u32 << (level - t.level as u32);
            let n = 1usize << level;
            let pi = self.page_index(t.face, level as u8);
            for dy in 0..span {
                for dx in 0..span {
                    let tile =
                        TileId { face: t.face, level: level as u8, x: (t.x as u32 * span + dx) as u16, y: (t.y as u32 * span + dy) as u16 };
                    let v = self.own_or_parent(tile);
                    self.page[pi][tile.y as usize * n + tile.x as usize] = v;
                }
            }
            self.dirty[pi] = true;
        }
    }

    /// Entry for a position (for tests and CPU-side queries).
    pub fn entry(&self, t: TileId) -> u32 {
        self.page[self.page_index(t.face, t.level)][t.y as usize * (1 << t.level) + t.x as usize]
    }

    /// Page-table slices that changed since the last call: `(face, level, entries)`.
    pub fn take_dirty(&mut self) -> Vec<(u32, u32, &[u32])> {
        let mut out = vec![];
        let detail = self.levels.saturating_sub(1);
        for (i, d) in self.dirty.iter_mut().enumerate() {
            if *d {
                *d = false;
                let face = i as u32 / detail;
                let level = i as u32 % detail + 1;
                out.push((face, level, &self.page[i][..]));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nodal_core::Face;

    #[test]
    fn page_table_tracks_best_ancestor() {
        let mut c = TileCache::new(4, 8);
        let a = TileId::new(Face::Front, 1, 0, 0);
        let leaf = TileId::new(Face::Front, 3, 1, 2);
        assert_eq!(c.entry(leaf), 0);
        let sa = c.allocate(a).unwrap();
        assert_eq!(c.entry(leaf), encode_entry(sa, 1));
        assert_eq!(c.entry(TileId::new(Face::Front, 3, 7, 7)), 0, "other quadrant untouched");
        let mid = TileId::new(Face::Front, 2, 0, 1);
        let sm = c.allocate(mid).unwrap();
        assert_eq!(c.entry(leaf), encode_entry(sm, 2));
        // Evicting the ancestor keeps the finer tile.
        c.begin_frame();
        c.touch(&mid);
        c.evict(a);
        assert_eq!(c.entry(leaf), encode_entry(sm, 2));
        assert_eq!(c.entry(TileId::new(Face::Front, 3, 3, 0)), 0);
        c.evict(mid);
        assert_eq!(c.entry(leaf), 0);
        assert!(!c.take_dirty().is_empty());
        assert!(c.take_dirty().is_empty());
    }

    #[test]
    fn lru_respects_touched_tiles() {
        let mut c = TileCache::new(3, 2);
        let t1 = TileId::new(Face::Up, 1, 0, 0);
        let t2 = TileId::new(Face::Up, 1, 1, 0);
        let t3 = TileId::new(Face::Up, 1, 0, 1);
        c.allocate(t1).unwrap();
        c.allocate(t2).unwrap();
        c.begin_frame();
        c.touch(&t1);
        c.touch(&t2);
        assert!(c.allocate(t3).is_none(), "everything visible, nothing to evict");
        c.begin_frame();
        c.touch(&t2);
        assert!(c.allocate(t3).is_some());
        assert!(!c.is_resident(&t1) && c.is_resident(&t2) && c.is_resident(&t3));
    }
}
