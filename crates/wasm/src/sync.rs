//! A drawing as changes: what changed since the last sync, and changes applied to it.
//!
//! The shape both ways is one JSON object of three maps whose values are JSON strings, as a Loro document keeps them
//! (one string per entry, so the other side stores and returns them without reading them):
//!
//! - `entities`: an entity's handle in hex → `{"z", "common", "kind"}`, or null when it's gone. `z` orders entities
//!   (draw order): low to high is back to front.
//! - `tables`: `<table>/<name>` (`layers/0`, `blocks/Door`, `constraints/7`, `parametric/`) → the entry, or null.
//! - `header`: a header variable's name → its value, or null. Variables that are each person's own (the current
//!   layer, colour and styles) and those about one device (handles, timers) stay out.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use cadcraft_engine::doc::{Drawing, Entity, Handle};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// Header variables that are each person's own, or a device's: never synced.
const LOCAL_VARS: &[&str] = &[
    "CLAYER",
    "CECOLOR",
    "CELTYPE",
    "CELWEIGHT",
    "CETRANSPARENCY",
    "CELTSCALE",
    "TEXTSTYLE",
    "DIMSTYLE",
    "CMLSTYLE",
    "CMLEADERSTYLE",
    "CTABLESTYLE",
    "CANNOSCALE",
    "HANDSEED",
    "TDUPDATE",
    "TDUUPDATE",
    "TDINDWG",
    "TDUSRTIMER",
    "EXTMIN",
    "EXTMAX",
    "PEXTMIN",
    "PEXTMAX",
    "VIEWCTR",
    "VIEWSIZE",
    "ORTHOMODE",
    "SNAPMODE",
    "GRIDMODE",
    "CVPORT",
];

/// Changes, as the document keeps them: each value a JSON string, null for a removal.
#[derive(Default, Serialize, Deserialize)]
pub struct Changes {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub entities: BTreeMap<String, Option<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tables: BTreeMap<String, Option<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub header: BTreeMap<String, Option<String>>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty() && self.tables.is_empty() && self.header.is_empty()
    }
}

/// An entity as the document keeps it: its properties without those at their defaults (most are), to keep the
/// document small.
#[derive(Serialize)]
struct Stored<'a> {
    z: f64,
    #[serde(skip_serializing_if = "serde_json::Map::is_empty")]
    common: serde_json::Map<String, serde_json::Value>,
    kind: &'a cadcraft_engine::doc::EntityKind,
}

fn compact(common: &cadcraft_engine::doc::Common) -> serde_json::Map<String, serde_json::Value> {
    static DEFAULT: std::sync::OnceLock<serde_json::Map<String, serde_json::Value>> = std::sync::OnceLock::new();
    let default = DEFAULT.get_or_init(|| match serde_json::to_value(cadcraft_engine::doc::Common::default()) {
        Ok(serde_json::Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    });
    match serde_json::to_value(common) {
        Ok(serde_json::Value::Object(mut m)) => {
            m.retain(|k, v| default.get(k) != Some(v));
            m
        }
        _ => serde_json::Map::new(),
    }
}

#[derive(Deserialize)]
struct Read {
    z: f64,
    #[serde(default)]
    common: cadcraft_engine::doc::Common,
    kind: cadcraft_engine::doc::EntityKind,
}

pub fn hex(h: Handle) -> String {
    format!("{:X}", h.0)
}

fn handle_of(key: &str) -> Option<Handle> {
    u64::from_str_radix(key, 16).ok().map(Handle)
}

/// How many handles a device has of its own: its range starts at its base.
pub const RANGE: u64 = 1 << 32;

/// What was last synced, and each entity's place in the draw order.
pub struct Synced {
    pub last: Arc<Drawing>,
    /// Each entity's `z`, and the handles by `z` (as ordered keys).
    z: HashMap<Handle, f64>,
    order: BTreeMap<u64, Handle>,
    /// The first handle of this device's range, and the next one it gives.
    base: u64,
    pub next: u64,
}

/// A key that sorts as `z` does.
fn zkey(z: f64) -> u64 {
    let bits = z.to_bits();
    if bits >> 63 == 1 { !bits } else { bits | (1 << 63) }
}

impl Synced {
    pub fn new(doc: Arc<Drawing>, base: u64) -> Self {
        Synced { last: doc, z: HashMap::new(), order: BTreeMap::new(), base, next: base }
    }

    fn set_z(&mut self, h: Handle, z: f64) {
        if let Some(old) = self.z.insert(h, z) {
            self.order.remove(&zkey(old));
        }
        self.order.insert(zkey(z), h);
    }

    fn drop_z(&mut self, h: Handle) {
        if let Some(old) = self.z.remove(&h) {
            self.order.remove(&zkey(old));
        }
    }

    /// Forgets every entity's place (a drawing opened from a file starts afresh).
    pub fn reset(&mut self, doc: Arc<Drawing>) {
        self.z.clear();
        self.order.clear();
        self.last = Arc::new(empty_like(&doc));
    }

    /// Gives the drawing this device's next handle, whatever changes from elsewhere bumped it to.
    pub fn own_handles(&self, doc: &mut Drawing) {
        doc.handseed = self.next;
    }

    /// What changed in `doc` since the last sync; `doc` is then the last synced.
    pub fn take(&mut self, doc: &Arc<Drawing>) -> Changes {
        let mut out = Changes::default();
        if Arc::ptr_eq(doc, &self.last) {
            return out;
        }
        if (self.base..self.base + RANGE).contains(&doc.handseed) {
            self.next = self.next.max(doc.handseed);
        }
        self.take_entities(doc, &mut out);
        let last = self.last.clone();
        take_tables(&last, doc, &mut out.tables);
        take_header(&last, doc, &mut out.header);
        self.last = doc.clone();
        out
    }

    fn take_entities(&mut self, doc: &Drawing, out: &mut Changes) {
        let last = self.last.clone();
        let (old, new) = (&last.model, &doc.model);
        if old.same_as(new) {
            return;
        }
        let old_chunks: Vec<&[Arc<Entity>]> = old.chunk_slices().map(|(_, c)| c).collect();
        let new_chunks: Vec<&[Arc<Entity>]> = new.chunk_slices().map(|(_, c)| c).collect();
        let same = |i: usize| old_chunks.get(i).is_some_and(|o| new_chunks.get(i).is_some_and(|n| std::ptr::eq(*o, *n)));
        // Gone: in a chunk that changed, and no longer anywhere.
        for (i, chunk) in old_chunks.iter().enumerate() {
            if same(i) {
                continue;
            }
            for e in chunk.iter() {
                if !new.contains(e.handle) {
                    self.drop_z(e.handle);
                    out.entities.insert(hex(e.handle), None);
                }
            }
        }
        // New, changed or moved in the draw order: walked in order, keeping each `z` above the one before.
        let all: Vec<&Arc<Entity>> = new.iter().collect();
        let mut prev = f64::NEG_INFINITY;
        let mut at = 0;
        for (i, chunk) in new_chunks.iter().enumerate() {
            if same(i) {
                if let Some(last) = chunk.last() {
                    prev = self.z.get(&last.handle).copied().unwrap_or(prev);
                }
                at += chunk.len();
                continue;
            }
            for e in chunk.iter() {
                let known = self.z.get(&e.handle).copied();
                let changed = match old.get(e.handle) {
                    Some(o) => !Arc::ptr_eq(o, e) && **o != **e,
                    None => true,
                };
                let z = match known {
                    Some(z) if z > prev => z,
                    _ => self.z_between(prev, &all[at + 1..]),
                };
                if changed || known != Some(z) {
                    self.set_z(e.handle, z);
                    let stored = Stored { z, common: compact(&e.common), kind: &e.kind };
                    out.entities.insert(hex(e.handle), serde_json::to_string(&stored).ok());
                }
                prev = z;
                at += 1;
            }
        }
    }

    /// A `z` above `prev` and below the next entity's that is in order, looking a little way ahead.
    fn z_between(&self, prev: f64, after: &[&Arc<Entity>]) -> f64 {
        let next = after.iter().take(64).find_map(|e| self.z.get(&e.handle).copied().filter(|z| *z > prev));
        match (prev.is_finite(), next) {
            (true, Some(n)) => prev + (n - prev) / 2.0,
            (false, Some(n)) => n - 1.0,
            (true, None) => prev + 1.0,
            (false, None) => 0.0,
        }
    }

    /// Applies `changes` to `doc`. With `synced`, they came from the document: they become the last synced, so they
    /// aren't sent back; without, they're this device's own edits, sent at the next `take`.
    pub fn apply(&mut self, doc: &mut Drawing, changes: &Changes) -> Result<(), String> {
        self.apply_entities(doc, &changes.entities)?;
        apply_tables(doc, &changes.tables)?;
        apply_header(doc, &changes.header)?;
        self.own_handles(doc);
        Ok(())
    }

    fn apply_entities(&mut self, doc: &mut Drawing, entities: &BTreeMap<String, Option<String>>) -> Result<(), String> {
        let mut placed: Vec<(f64, Entity)> = Vec::new();
        for (key, value) in entities {
            let Some(h) = handle_of(key) else { continue };
            match value {
                None => {
                    doc.model.remove(h);
                    self.drop_z(h);
                }
                Some(text) => {
                    let read: Read = serde_json::from_str(text).map_err(|e| format!("entity {key}: {e}"))?;
                    let e = Entity { handle: h, common: read.common, kind: read.kind };
                    if self.z.get(&h) == Some(&read.z) && doc.model.contains(h) {
                        doc.model.replace(e);
                    } else {
                        doc.model.remove(h);
                        self.drop_z(h);
                        placed.push((read.z, e));
                    }
                }
            }
        }
        placed.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (z, e) in placed {
            let h = e.handle;
            let below = self.order.range(..zkey(z)).next_back().map(|(_, h)| *h);
            let top = self.order.last_key_value().map(|(_, h)| *h);
            match below {
                Some(b) if Some(b) != top => {
                    doc.model.push(e);
                    doc.model.move_next_to(h, b, true);
                }
                Some(_) => doc.model.push(e),
                None => doc.model.push_front(e),
            }
            self.set_z(h, z);
        }
        Ok(())
    }

    /// Marks `doc` as synced: what it holds now is what the document holds.
    pub fn synced(&mut self, doc: &Arc<Drawing>) {
        self.last = doc.clone();
    }
}

/// `doc` without entities, tables entries or header variables: what a drawing opened from a file is compared against,
/// so all of it is new.
fn empty_like(doc: &Drawing) -> Drawing {
    let mut d = doc.clone();
    d.model = Default::default();
    d.layers.clear();
    d.linetypes.clear();
    d.text_styles.clear();
    d.dim_styles.clear();
    d.mleader_styles.clear();
    d.table_styles.clear();
    d.mline_styles.clear();
    d.blocks.clear();
    d.layouts.clear();
    d.views.clear();
    d.ucss.clear();
    d.layer_states.clear();
    d.groups.clear();
    d.plot_style_tables.clear();
    d.constraints.clear();
    d.scales.clear();
    d.parametric = Default::default();
    d.header.vars.clear();
    d
}

// ---- Tables: each a list of entries by name (constraints by number), blocks a map.

/// Calls `each!(field, "name", key)` for every table kept by name.
macro_rules! named_tables {
    ($each:ident) => {
        $each!(layers, "layers", |t| t.name.clone());
        $each!(linetypes, "linetypes", |t| t.name.clone());
        $each!(text_styles, "textStyles", |t| t.name.clone());
        $each!(dim_styles, "dimStyles", |t| t.name.clone());
        $each!(mleader_styles, "mleaderStyles", |t| t.name.clone());
        $each!(table_styles, "tableStyles", |t| t.name.clone());
        $each!(mline_styles, "mlineStyles", |t| t.name.clone());
        $each!(layouts, "layouts", |t| t.name.clone());
        $each!(views, "views", |t| t.name.clone());
        $each!(ucss, "ucss", |t| t.name.clone());
        $each!(layer_states, "layerStates", |t| t.name.clone());
        $each!(groups, "groups", |t| t.name.clone());
        $each!(plot_style_tables, "plotStyles", |t| t.name.clone());
        $each!(scales, "scales", |t| t.name.clone());
        $each!(constraints, "constraints", |t| t.id.to_string());
    };
}

fn keyed<T: Serialize>(items: &[T], key: impl Fn(&T) -> String) -> BTreeMap<String, String> {
    items.iter().filter_map(|t| serde_json::to_string(t).ok().map(|s| (key(t), s))).collect()
}

fn diff(table: &str, old: BTreeMap<String, String>, new: BTreeMap<String, String>, out: &mut BTreeMap<String, Option<String>>) {
    for key in old.keys() {
        if !new.contains_key(key) {
            out.insert(format!("{table}/{key}"), None);
        }
    }
    for (key, value) in new {
        if old.get(&key) != Some(&value) {
            out.insert(format!("{table}/{key}"), Some(value));
        }
    }
}

fn take_tables(old: &Drawing, new: &Drawing, out: &mut BTreeMap<String, Option<String>>) {
    macro_rules! each {
        ($field:ident, $name:literal, $key:expr) => {
            if old.$field != new.$field {
                diff($name, keyed(&old.$field, $key), keyed(&new.$field, $key), out);
            }
        };
    }
    named_tables!(each);
    if old.blocks != new.blocks {
        for name in old.blocks.keys() {
            if !new.blocks.contains_key(name) {
                out.insert(format!("blocks/{name}"), None);
            }
        }
        for (name, block) in &new.blocks {
            if old.blocks.get(name).is_none_or(|o| !Arc::ptr_eq(o, block) && o != block) {
                out.insert(format!("blocks/{name}"), serde_json::to_string(&**block).ok());
            }
        }
    }
    if old.parametric != new.parametric {
        out.insert("parametric/".into(), serde_json::to_string(&new.parametric).ok());
    }
}

fn apply_to<T: DeserializeOwned>(items: &mut Vec<T>, key: impl Fn(&T) -> String, name: &str, value: Option<&String>) -> Result<(), String> {
    let at = items.iter().position(|t| key(t) == name);
    match (value, at) {
        (None, Some(i)) => {
            items.remove(i);
        }
        (None, None) => {}
        (Some(text), at) => {
            let item: T = serde_json::from_str(text).map_err(|e| format!("{name}: {e}"))?;
            match at {
                Some(i) => items[i] = item,
                None => items.push(item),
            }
        }
    }
    Ok(())
}

fn apply_tables(doc: &mut Drawing, tables: &BTreeMap<String, Option<String>>) -> Result<(), String> {
    for (path, value) in tables {
        let Some((table, name)) = path.split_once('/') else { continue };
        macro_rules! each {
            ($field:ident, $name:literal, $key:expr) => {
                if table == $name {
                    apply_to(&mut doc.$field, $key, name, value.as_ref())?;
                    continue;
                }
            };
        }
        named_tables!(each);
        match (table, value) {
            ("blocks", None) => {
                doc.blocks.remove(name);
            }
            ("blocks", Some(text)) => {
                let block = serde_json::from_str(text).map_err(|e| format!("block {name}: {e}"))?;
                doc.blocks.insert(name.to_string(), Arc::new(block));
            }
            ("parametric", Some(text)) => {
                doc.parametric = serde_json::from_str(text).map_err(|e| format!("parameters: {e}"))?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn take_header(old: &Drawing, new: &Drawing, out: &mut BTreeMap<String, Option<String>>) {
    if old.header == new.header {
        return;
    }
    let shared = |k: &String| !LOCAL_VARS.contains(&k.as_str());
    for key in old.header.vars.keys().filter(|k| shared(k)) {
        if !new.header.vars.contains_key(key) {
            out.insert(key.clone(), None);
        }
    }
    for (key, value) in new.header.vars.iter().filter(|(k, _)| shared(k)) {
        if old.header.vars.get(key) != Some(value) {
            out.insert(key.clone(), serde_json::to_string(value).ok());
        }
    }
}

fn apply_header(doc: &mut Drawing, header: &BTreeMap<String, Option<String>>) -> Result<(), String> {
    for (key, value) in header {
        if LOCAL_VARS.contains(&key.as_str()) {
            continue;
        }
        match value {
            None => {
                doc.header.vars.remove(key);
            }
            Some(text) => {
                let v = serde_json::from_str(text).map_err(|e| format!("{key}: {e}"))?;
                doc.header.vars.insert(key.clone(), v);
            }
        }
    }
    Ok(())
}

/// Changes read from JSON whose values may be strings (as the document keeps them) or raw JSON.
pub fn read_changes(json: &str) -> Result<Changes, String> {
    #[derive(Deserialize)]
    struct Loose {
        #[serde(default)]
        entities: BTreeMap<String, Option<Box<RawValue>>>,
        #[serde(default)]
        tables: BTreeMap<String, Option<Box<RawValue>>>,
        #[serde(default)]
        header: BTreeMap<String, Option<Box<RawValue>>>,
    }
    fn texts(m: BTreeMap<String, Option<Box<RawValue>>>) -> BTreeMap<String, Option<String>> {
        m.into_iter()
            .map(|(k, v)| {
                let v = v.map(|raw| serde_json::from_str::<String>(raw.get()).unwrap_or_else(|_| raw.get().to_string()));
                (k, v)
            })
            .collect()
    }
    let loose: Loose = serde_json::from_str(json).map_err(|e| e.to_string())?;
    Ok(Changes { entities: texts(loose.entities), tables: texts(loose.tables), header: texts(loose.header) })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use cadcraft_engine::Session;

    /// A session and what it last synced, as a device holds a drawing.
    struct Device {
        s: Session,
        synced: Synced,
    }

    impl Device {
        fn new(base: u64) -> Self {
            let mut s = Session::new();
            s.doc_mut().unwrap().handseed = base;
            let doc = s.state().unwrap().doc.clone();
            Device { synced: Synced::new(Arc::new(empty_like(&doc)), base), s }
        }
        fn take(&mut self) -> Changes {
            let doc = self.s.state().unwrap().doc.clone();
            self.synced.take(&doc)
        }
        fn apply(&mut self, c: &Changes) {
            let doc = self.s.doc_mut().unwrap();
            self.synced.apply(doc, c).unwrap();
            let doc = self.s.state().unwrap().doc.clone();
            self.synced.synced(&doc);
        }
        fn handles(&self) -> Vec<u64> {
            self.s.doc().unwrap().model.iter().map(|e| e.handle.0).collect()
        }
    }

    #[test]
    fn a_drawing_made_on_one_device_arrives_whole_on_another() {
        let mut ana = Device::new(1 << 32);
        let mut ben = Device::new(2 << 32);
        ben.take();
        ana.s.cmdline("line 0,0 10,0 10,10 ").unwrap();
        ana.s.cmdline("circle 5,5 3").unwrap();
        let c = ana.take();
        assert_eq!(c.entities.len(), 3);
        assert!(c.tables.keys().any(|k| k == "layers/0"));
        ben.apply(&c);
        assert_eq!(ben.handles(), ana.handles());
        assert!(ben.handles().iter().all(|h| *h >= 1 << 32 && *h < 2 << 32));
        // Nothing comes back from what was applied.
        assert!(ben.take().is_empty());
    }

    #[test]
    fn two_devices_drawing_at_once_never_share_a_handle_and_keep_both() {
        let mut ana = Device::new(1 << 32);
        let mut ben = Device::new(2 << 32);
        let first = ana.take();
        ben.apply(&first);
        ana.s.cmdline("line 0,0 1,1 ").unwrap();
        ben.s.cmdline("line 5,5 6,6 ").unwrap();
        let (a, b) = (ana.take(), ben.take());
        ana.apply(&b);
        ben.apply(&a);
        assert_eq!(ana.handles().len(), 2);
        let (mut x, mut y) = (ana.handles(), ben.handles());
        x.sort_unstable();
        y.sort_unstable();
        assert_eq!(x, y);
        // Each goes on with its own range after taking the other's.
        ben.s.cmdline("line 9,9 8,8 ").unwrap();
        let c = ben.take();
        assert!(c.entities.keys().all(|k| u64::from_str_radix(k, 16).unwrap() >> 32 == 2));
    }

    #[test]
    fn erasing_moving_and_reordering_travel() {
        let mut ana = Device::new(1 << 32);
        let mut ben = Device::new(2 << 32);
        for i in 0..5 {
            ana.s.cmdline(&format!("line {i},0 {i},5 ")).unwrap();
        }
        ben.apply(&ana.take());
        let hs = ana.handles();
        // Bring the first to the front, erase the second, move the third.
        ana.s.doc_mut().unwrap().model.bring_to_front(Handle(hs[0]));
        ana.s.doc_mut().unwrap().model.remove(Handle(hs[1]));
        ana.s.doc_mut().unwrap().model.modify(Handle(hs[2]), |e| e.common.layer = "Walls".into());
        let c = ana.take();
        assert_eq!(c.entities.get(&hex(Handle(hs[1]))), Some(&None));
        ben.apply(&c);
        assert_eq!(ben.handles(), ana.handles());
        assert_eq!(ben.s.doc().unwrap().entity(Handle(hs[2])).unwrap().common.layer, "Walls");
        // Send one to the back.
        ana.s.doc_mut().unwrap().model.send_to_back(Handle(hs[4]));
        ben.apply(&ana.take());
        assert_eq!(ben.handles(), ana.handles());
    }

    #[test]
    fn each_persons_current_layer_stays_theirs() {
        let mut ana = Device::new(1 << 32);
        let mut ben = Device::new(2 << 32);
        ben.apply(&ana.take());
        ana.s.cmdline("-layer m Walls ").unwrap();
        ana.s.cmdline("ltscale 2").unwrap();
        let c = ana.take();
        assert!(c.tables.contains_key("layers/Walls"));
        assert!(!c.header.contains_key("CLAYER"));
        assert!(c.header.contains_key("LTSCALE"));
        ben.apply(&c);
        assert_eq!(ben.s.doc().unwrap().header.str("CLAYER", "0"), "0");
        assert!(ben.s.doc().unwrap().layers.iter().any(|l| l.name == "Walls"));
    }

    #[test]
    fn changes_read_from_the_documents_strings() {
        let mut ana = Device::new(1 << 32);
        ana.s.cmdline("circle 0,0 2").unwrap();
        let text = serde_json::to_string(&ana.take()).unwrap();
        let back = read_changes(&text).unwrap();
        let mut ben = Device::new(2 << 32);
        ben.apply(&back);
        assert_eq!(ben.handles().len(), 1);
    }
}
