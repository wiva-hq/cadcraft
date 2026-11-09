//! A display list as flat arrays for a canvas: vertices relative to an origin (so `f32` keeps its precision far from
//! zero), triangles, and one record per primitive.

use cadcraft_engine::render::{DisplayList, Kind};
use wasm_bindgen::prelude::*;

/// u32s per primitive in [`Scene::prims`].
pub const PRIM: usize = 6;

/// What a primitive draws, as [`Scene::prims`] numbers it.
fn kind_code(kind: Kind) -> u32 {
    match kind {
        Kind::Polyline => 0,
        Kind::Tris => 1,
        Kind::Mask => 2,
        Kind::Point => 3,
        Kind::Infinite { ray: false } => 4,
        Kind::Infinite { ray: true } => 5,
        Kind::Image { .. } => 6,
    }
}

/// A display list for drawing. Each primitive is six u32s: the entity's handle (low, high 32 bits), its colour
/// (`0xRRGGBB`, with bit 24 set for colour 7, drawn in the text's colour), its kind (bits 0–7: 0 polyline, 1
/// triangles, 2 mask, 3 point, 4 line, 5 ray, 6 image frame) with its transparency in percent (bits 8–15) and
/// lineweight in hundredths of a mm (bits 16–31), and where its points start and how many there are, in `verts` (or
/// `tris` for triangles and masks).
#[wasm_bindgen]
pub struct Scene {
    origin: [f64; 2],
    bounds: [f64; 4],
    verts: Vec<f32>,
    tris: Vec<f32>,
    prims: Vec<u32>,
}

#[wasm_bindgen]
impl Scene {
    /// The origin every point is relative to: `[x, y]`.
    pub fn origin(&self) -> Vec<f64> {
        self.origin.to_vec()
    }
    /// What it covers, in drawing units: `[minX, minY, maxX, maxY]` (empty: min above max).
    pub fn bounds(&self) -> Vec<f64> {
        self.bounds.to_vec()
    }
    pub fn verts(&self) -> Vec<f32> {
        self.verts.clone()
    }
    pub fn tris(&self) -> Vec<f32> {
        self.tris.clone()
    }
    pub fn prims(&self) -> Vec<u32> {
        self.prims.clone()
    }
}

impl Scene {
    pub fn origin_xy(&self) -> [f64; 2] {
        self.origin
    }

    /// One scene of several display lists, drawn in their order (a drawing's chunks), relative to `origin` (or the
    /// middle of what they cover).
    pub fn of(lists: &[&DisplayList], origin: Option<[f64; 2]>) -> Scene {
        let mut b = cadcraft_engine::geom::Bounds2::EMPTY;
        for list in lists {
            b = b.union(&list.bounds);
        }
        let empty = !(b.min.x <= b.max.x && b.min.y <= b.max.y);
        let origin = origin.unwrap_or(if empty { [0.0, 0.0] } else { [(b.min.x + b.max.x) / 2.0, (b.min.y + b.max.y) / 2.0] });
        let rel = |v: &cadcraft_engine::geom::Vec2| [(v.x - origin[0]) as f32, (v.y - origin[1]) as f32];
        let (mut nv, mut nt, mut np) = (0, 0, 0);
        for list in lists {
            nv += list.verts.len();
            nt += list.tris.len();
            np += list.prims.len();
        }
        let mut verts = Vec::with_capacity(nv * 2);
        let mut tris = Vec::with_capacity(nt * 2);
        let mut prims = Vec::with_capacity(np * PRIM);
        for list in lists {
            let (v0, t0) = ((verts.len() / 2) as u32, (tris.len() / 2) as u32);
            for v in &list.verts {
                verts.extend(rel(v));
            }
            for v in &list.tris {
                tris.extend(rel(v));
            }
            for p in &list.prims {
                let (r, g, bl) = (u32::from(p.color.0), u32::from(p.color.1), u32::from(p.color.2));
                let color = (r << 16) | (g << 8) | bl | if p.aci7 { 1 << 24 } else { 0 };
                let lw = ((p.lw.max(0.0) * 100.0).round() as u32).min(0xFFFF);
                let look = kind_code(p.kind) | (u32::from(p.transparency.min(100)) << 8) | (lw << 16);
                let start = p.start + if matches!(p.kind, Kind::Tris | Kind::Mask) { t0 } else { v0 };
                // An infinite line's direction is a unit vector, not a point: kept as it is.
                if matches!(p.kind, Kind::Infinite { .. })
                    && let Some(d) = list.verts.get(p.start as usize + 1)
                {
                    let at = (start as usize + 1) * 2;
                    if let Some(slot) = verts.get_mut(at..at + 2) {
                        slot.copy_from_slice(&[d.x as f32, d.y as f32]);
                    }
                }
                prims.extend([p.handle.0 as u32, (p.handle.0 >> 32) as u32, color, look, start, p.len]);
            }
        }
        let bounds = if empty { [1.0, 1.0, 0.0, 0.0] } else { [b.min.x, b.min.y, b.max.x, b.max.y] };
        Scene { origin, bounds, verts, tris, prims }
    }
}
