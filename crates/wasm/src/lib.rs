//! CADCraft's engine for Forest, in WebAssembly.
//!
//! One [`Engine`] is one drawing as a person works on it: a CADCraft session (its commands, prompts, snaps, selection
//! and grips) whose drawing follows a Loro document. The document is the truth: `load` and `apply` put its entries
//! into the drawing, and after every step a person takes, `take` gives what the step changed, to write back
//! (`sync.rs`). Drawing is the page's: `scene` gives the display list as flat arrays (`scene.rs`).
//!
//! Undo is the document's (each person's own changes), not the session's: the session's undo stack is cleared after
//! every step.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod scene;
mod sync;

use std::sync::Arc;

use cadcraft_engine::doc::{Drawing, Handle, Space};
use cadcraft_engine::geom::Vec2;
use cadcraft_engine::render::{self, Options};
use cadcraft_engine::snap::SnapHit;
use cadcraft_engine::snap::tracking::CursorQuery;
use cadcraft_engine::{Input, Session, View, sysvars};
use serde_json::{Value, json};
use wasm_bindgen::prelude::*;

pub use scene::Scene;
use sync::{Synced, hex, read_changes};

fn fail(e: impl std::fmt::Display) -> JsError {
    JsError::new(&e.to_string())
}

/// A drawing being worked on: the engine's session, following a document.
#[wasm_bindgen]
pub struct Engine {
    session: Session,
    synced: Synced,
    /// The snap under the cursor at the last `cursor`, which a click there takes.
    snap: Option<SnapHit>,
    /// The log lines already handed out.
    told: usize,
    /// Model space's display lists, one per chunk of the drawing's store (`chunks`).
    cache: Chunks,
}

/// Each chunk's display list, kept while the drawing it was built from (`doc`) looks the same outside its entities and
/// the tolerance is the same: an edit rebuilds only the chunks it touched.
#[derive(Default)]
struct Chunks {
    doc: Option<Arc<Drawing>>,
    tolerance: f64,
    dash: f64,
    lists: std::collections::HashMap<usize, render::DisplayList>,
    /// The origin scenes are relative to, kept so a drawing's scenes line up.
    origin: Option<[f64; 2]>,
}

/// Whether two drawings draw their entities alike: the same tables, blocks and variables.
fn same_look(a: &Drawing, b: &Drawing) -> bool {
    a.header == b.header
        && a.layers == b.layers
        && a.linetypes == b.linetypes
        && a.text_styles == b.text_styles
        && a.dim_styles == b.dim_styles
        && a.mleader_styles == b.mleader_styles
        && a.table_styles == b.table_styles
        && a.mline_styles == b.mline_styles
        && a.layouts == b.layouts
        && a.hidden_objects == b.hidden_objects
        && a.plot_style_tables == b.plot_style_tables
        && a.blocks.len() == b.blocks.len()
        && a.blocks.iter().zip(&b.blocks).all(|((ka, va), (kb, vb))| ka == kb && (Arc::ptr_eq(va, vb) || va == vb))
}

#[wasm_bindgen]
impl Engine {
    /// A new, empty drawing. `base` is the first handle of this device's own range (a multiple of 2^32 below 2^52),
    /// so drawings made on two devices at once never give two objects one handle.
    #[wasm_bindgen(constructor)]
    pub fn new(base: f64) -> Result<Engine, JsError> {
        let mut session = Session::new();
        let base = base as u64;
        let doc = session.doc_mut().map_err(fail)?;
        doc.handseed = doc.handseed.max(base);
        let doc = session.state().map_err(fail)?.doc.clone();
        let synced = Synced::new(doc, base);
        Ok(Engine { session, synced, snap: None, told: 0, cache: Chunks::default() })
    }

    // ---- Following the document.

    /// Applies changes from the document (`sync.rs`'s shape); they aren't sent back.
    pub fn apply(&mut self, changes: &str) -> Result<(), JsError> {
        let changes = read_changes(changes).map_err(fail)?;
        if changes.is_empty() {
            return Ok(());
        }
        let mut synced = std::mem::replace(&mut self.synced, Synced::new(Arc::new(Drawing::default()), 0));
        let done = (|| {
            let doc = self.session.doc_mut().map_err(fail)?;
            synced.apply(doc, &changes).map_err(fail)
        })();
        if let Ok(st) = self.session.state() {
            synced.synced(&st.doc.clone());
        }
        self.synced = synced;
        self.forget_undo();
        done
    }

    /// Applies changes this device makes outside a command (a layer's colour, a property): sent at the next `take`.
    pub fn edit(&mut self, changes: &str) -> Result<(), JsError> {
        let changes = read_changes(changes).map_err(fail)?;
        let mut synced = std::mem::replace(&mut self.synced, Synced::new(Arc::new(Drawing::default()), 0));
        let done = (|| {
            let doc = self.session.doc_mut().map_err(fail)?;
            synced.apply(doc, &changes).map_err(fail)
        })();
        self.synced = synced;
        done
    }

    /// What changed since the last `take` or `apply`, to write to the document: JSON, or "" when nothing did.
    pub fn take(&mut self) -> Result<String, JsError> {
        let doc = self.session.state().map_err(fail)?.doc.clone();
        let changes = self.synced.take(&doc);
        self.forget_undo();
        if changes.is_empty() {
            return Ok(String::new());
        }
        serde_json::to_string(&changes).map_err(fail)
    }

    fn forget_undo(&mut self) {
        if let Ok(st) = self.session.state_mut() {
            st.undo.clear();
            st.redo.clear();
        }
    }

    // ---- Files.

    /// Opens a drawing from a file's bytes (DXF, or SVG), in place of this one: all of it is new at the next `take`.
    /// DWG goes through DXF first (the app's native side converts it).
    pub fn open(&mut self, bytes: &[u8], name: &str) -> Result<String, JsError> {
        let mut doc = cadcraft_io::read(bytes, name).map_err(fail)?;
        let notes = std::mem::take(&mut doc.read_notes);
        let st = self.session.state_mut().map_err(fail)?;
        st.doc = Arc::new(doc);
        st.selection.clear();
        let doc = st.doc.clone();
        self.synced.reset(doc);
        self.cache = Chunks::default();
        let doc = self.session.doc_mut().map_err(fail)?;
        doc.handseed = doc.handseed.max(self.synced.next);
        self.synced.next = doc.handseed;
        let _ = self.session.zoom_extents();
        serde_json::to_string(&notes).map_err(fail)
    }

    /// The drawing as a file named `name`: DXF (`.dxf`), PDF, SVG or PNG (model space fitted to its extents).
    pub fn save(&self, name: &str) -> Result<Vec<u8>, JsError> {
        cadcraft_io::write(self.session.doc().map_err(fail)?, name).map_err(fail)
    }

    /// A picture of the drawing's model space, `width` by `height` pixels, as a PNG.
    pub fn png(&self, width: u32, height: u32) -> Result<Vec<u8>, JsError> {
        cadcraft_io::png(self.session.doc().map_err(fail)?, &Space::Model, width.clamp(1, 4096), height.clamp(1, 4096)).map_err(fail)
    }

    // ---- Commands and input.

    /// A line typed at the command line: a command, its options, points (`10,20`, `@5<45`), numbers or text.
    pub fn command(&mut self, text: &str) {
        if let Err(e) = self.session.cmdline(text) {
            self.session.echo(e.to_string());
        }
    }

    /// Runs command `id` with JSON parameters, as menus and panels do; gives its result as JSON.
    pub fn execute(&mut self, id: &str, params: &str) -> Result<String, JsError> {
        let params: Value = if params.is_empty() { Value::Null } else { serde_json::from_str(params).map_err(fail)? };
        let out = self.session.execute(id, &params).map_err(fail)?;
        Ok(out.to_string())
    }

    /// Starts command `name` (its prompts follow).
    pub fn start(&mut self, name: &str) {
        if let Err(e) = self.session.start(name) {
            self.session.echo(e.to_string());
        }
    }

    /// A click at a point prompt, or with no command running (picks, or starts a selection window): at the cursor's
    /// snap when `cursor` found one there.
    pub fn click(&mut self, x: f64, y: f64, shift: bool) {
        let p = Vec2::new(x, y);
        let snap = self.snap.filter(|s| s.point.near(p, 1e-9 * (1.0 + p.len())));
        let done = if self.session.prompting() {
            match snap {
                Some(hit) => self.session.input_snapped(hit),
                None => self.session.input(Input::Point(p)),
            }
        } else {
            self.session.idle_click(p, shift)
        };
        if let Err(e) = done {
            self.session.echo(e.to_string());
        }
    }

    /// A selection window opened by pressing at `x, y` and dragging: true when one opened.
    pub fn window(&mut self, x: f64, y: f64) -> bool {
        self.session.begin_window(Vec2::new(x, y))
    }

    /// A selection window closed by letting go at `x, y`.
    pub fn window_end(&mut self, x: f64, y: f64, shift: bool) {
        if self.session.pending_window.is_none() {
            return;
        }
        let p = Vec2::new(x, y);
        let done = if self.session.prompting() { self.session.input(Input::Point(p)) } else { self.session.idle_click(p, shift) };
        if let Err(e) = done {
            self.session.echo(e.to_string());
        }
    }

    /// One of the prompt's keywords.
    pub fn keyword(&mut self, word: &str) {
        if let Err(e) = self.session.input(Input::Keyword(word.into())) {
            self.session.echo(e.to_string());
        }
    }

    /// Enter (or a right-click while a command runs): ends the command or takes the default; at `Command:`, repeats
    /// the last command.
    pub fn enter(&mut self) {
        if let Err(e) = self.session.input(Input::Enter) {
            self.session.echo(e.to_string());
        }
    }

    /// Esc: cancels the command, or with none, clears the selection.
    pub fn escape(&mut self) {
        if self.session.running.is_some() || self.session.pending_window.is_some() {
            self.session.cancel();
            self.session.pending_window = None;
        } else {
            self.session.set_selection(Vec::new());
        }
    }

    /// Starts editing grip `index` of `handle` (a click on it).
    pub fn grip(&mut self, handle: f64, index: u32) {
        if let Err(e) = self.session.start_grip_edit(&[(Handle(handle as u64), index as usize)]) {
            self.session.echo(e.to_string());
        }
    }

    /// Selects these objects (JSON handles), as picking them does.
    pub fn select(&mut self, handles: &str) -> Result<(), JsError> {
        let hs: Vec<u64> = serde_json::from_str(handles).map_err(fail)?;
        self.session.set_selection(hs.into_iter().map(Handle).collect());
        Ok(())
    }

    /// Sets a system variable (`ORTHOMODE`, `OSMODE`, `CLAYER`…) to a JSON value.
    pub fn set_var(&mut self, name: &str, value: &str) -> Result<(), JsError> {
        let v: Value = serde_json::from_str(value).map_err(fail)?;
        sysvars::set(&mut self.session, name, &v).map_err(fail)
    }

    /// A system variable's value as JSON (null: no such variable).
    pub fn var(&self, name: &str) -> String {
        sysvars::get(&self.session, name).unwrap_or(Value::Null).to_string()
    }

    // ---- The view.

    /// The canvas's size in pixels and the view (its centre and height in drawing units), as the page shows it.
    pub fn set_view(&mut self, width: f64, height: f64, cx: f64, cy: f64, view_height: f64) {
        self.session.viewport_px = (width.max(1.0), height.max(1.0));
        if let Ok(st) = self.session.state_mut()
            && view_height.is_finite()
            && view_height > 0.0
        {
            st.set_view_quiet(View { center: Vec2::new(cx, cy), height: view_height, twist: 0.0 });
        }
    }

    /// The view: `[cx, cy, height]` (commands such as ZOOM change it).
    pub fn view(&self) -> Vec<f64> {
        let v = self.session.state().map(|st| st.view()).unwrap_or_default();
        vec![v.center.x, v.center.y, v.height]
    }

    /// Fits the view to everything drawn.
    pub fn zoom_extents(&mut self) {
        let _ = self.session.zoom_extents();
    }

    /// The cursor at `x, y` (drawing units), `scale` pixels per unit: snapped (object snaps, tracking, grid, ortho,
    /// polar) when the prompt asks for a point. JSON: `{point, snap?, track?, wait}`.
    pub fn cursor(&mut self, x: f64, y: f64, scale: f64, shift: bool, now: f64) -> String {
        let raw = Vec2::new(x, y);
        let prompt = self.session.current_prompt();
        let wants = prompt.as_ref().is_some_and(cadcraft_engine::pointmod::accepts);
        let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
        let result = if wants && self.session.pending_window.is_none() {
            let q = CursorQuery {
                base: prompt.as_ref().and_then(|p| p.base),
                deferred: prompt.as_ref().is_some_and(|p| p.deferred),
                corner: prompt.as_ref().is_some_and(|p| p.corner),
                aperture: self.session.settings.aperture / scale,
                tol: 6.0 / scale,
                now,
                shift,
            };
            self.session.snap_cursor(raw, &q)
        } else {
            self.session.tracking.clear();
            cadcraft_engine::snap::tracking::CursorSnap { point: raw, ..Default::default() }
        };
        self.snap = result.snap;
        self.session.cursor = result.point;
        self.session.cursor_deferred = result.snap.and_then(|h| h.deferred);
        serde_json::to_string(&result).unwrap_or_default()
    }

    /// The object under `x, y` that a click would pick (null when nothing is, or picking isn't what a click does).
    pub fn hover(&self, x: f64, y: f64, scale: f64) -> Option<f64> {
        if !self.session.previews_selection() || self.session.pending_window.is_some() {
            return None;
        }
        let st = self.session.state().ok()?;
        let ap = self.session.settings.pickbox.max(1.0) * 1.5 / scale.max(1e-12);
        cadcraft_engine::select::pick(&st.doc, &st.edit_space(), Vec2::new(x, y), ap).map(|h| h.0 as f64)
    }

    // ---- What the page shows.

    /// Where things stand, as JSON: the prompt and its text, the command running, the selection, its grips, new log
    /// lines, the drafting aids and the current layer.
    pub fn status(&mut self) -> String {
        let s = &self.session;
        let selection = s.selection();
        let grips: Vec<Value> = if s.prompting() || !s.shows_grips(selection.len()) {
            Vec::new()
        } else {
            selection
                .iter()
                .flat_map(|h| s.grips_of(*h).unwrap_or_default().into_iter().enumerate().map(move |(i, p)| json!([h.0, i, p.x, p.y])))
                .collect()
        };
        let log: Vec<&String> = s.log.iter().skip(self.told.min(s.log.len())).collect();
        let st = s.state().ok();
        let space = match st.map(|st| st.space.clone()) {
            Some(Space::Paper(name)) => name,
            _ => String::new(),
        };
        let out = json!({
            "prompt": s.current_prompt(),
            "text": s.prompt_text(),
            "running": s.running.as_ref().map(|r| r.id.clone()),
            "selection": selection.iter().map(|h| h.0).collect::<Vec<_>>(),
            "grips": grips,
            "log": log,
            "window": s.pending_window.as_ref().map(|w| [w.corner.x, w.corner.y]),
            "space": space,
            "aids": {
                "ortho": s.settings.orthomode,
                "polar": s.settings.polarmode,
                "osnap": s.settings.osmode & 0x4000 == 0 && s.settings.osmode != 0,
                "osmode": s.settings.osmode,
                "otrack": s.settings.otrack,
                "grid": s.settings.gridmode,
                "snap": s.settings.snapmode,
                "lineweights": s.settings.lwdisplay,
            },
            "layer": st.map(|st| st.doc.header.str("CLAYER", "0")).unwrap_or_default(),
        });
        self.told = self.session.log.len();
        out.to_string()
    }

    /// The drawing's tables, as panels list them (JSON): layers whole, the rest by name.
    pub fn tables(&self) -> String {
        let Ok(d) = self.session.doc() else { return "{}".into() };
        json!({
            "layers": d.layers,
            "linetypes": d.linetypes.iter().map(|t| &t.name).collect::<Vec<_>>(),
            "textStyles": d.text_styles.iter().map(|t| &t.name).collect::<Vec<_>>(),
            "dimStyles": d.dim_styles.iter().map(|t| &t.name).collect::<Vec<_>>(),
            "blocks": d.blocks.keys().filter(|n| !n.starts_with('*')).collect::<Vec<_>>(),
            "layouts": d.layouts.iter().map(|t| &t.name).collect::<Vec<_>>(),
            "count": d.model.len(),
        })
        .to_string()
    }

    /// These objects (JSON handles) as JSON: each `{handle, common, kind}`.
    pub fn entities(&self, handles: &str) -> Result<String, JsError> {
        let hs: Vec<u64> = serde_json::from_str(handles).map_err(fail)?;
        let d = self.session.doc().map_err(fail)?;
        let found: Vec<_> = hs.into_iter().filter_map(|h| d.entity(Handle(h))).collect();
        serde_json::to_string(&found).map_err(fail)
    }

    /// Changes these objects' (JSON handles) properties: `patch` is a JSON object of `Common`'s fields (`layer`,
    /// `color`, `linetype`, `lineweight`, `transparency`, `ltscale`), merged into each. Sent at the next `take`.
    pub fn patch(&mut self, handles: &str, patch: &str) -> Result<(), JsError> {
        let hs: Vec<u64> = serde_json::from_str(handles).map_err(fail)?;
        let Value::Object(patch) = serde_json::from_str::<Value>(patch).map_err(fail)? else { return Err(fail("a patch is an object")) };
        let doc = self.session.doc_mut().map_err(fail)?;
        for h in hs {
            let Some(e) = doc.entity(Handle(h)) else { continue };
            let Value::Object(mut common) = serde_json::to_value(&e.common).map_err(fail)? else { continue };
            common.extend(patch.clone());
            let common = serde_json::from_value(Value::Object(common)).map_err(fail)?;
            doc.model.modify(Handle(h), |e| e.common = common);
        }
        Ok(())
    }

    /// Changes layer `name`: `patch` is a JSON object of its fields (`color`, `linetype`, `lineweight`, `on`, `frozen`,
    /// `locked`, `plot`, `transparency`, `description`); a new name renames it everywhere it's used. Sent at the next
    /// `take`.
    pub fn patch_layer(&mut self, name: &str, patch: &str) -> Result<(), JsError> {
        let Value::Object(patch) = serde_json::from_str::<Value>(patch).map_err(fail)? else { return Err(fail("a patch is an object")) };
        let renamed = patch.get("name").and_then(Value::as_str).filter(|n| *n != name).map(str::to_string);
        if let Some(to) = &renamed {
            self.session.execute("rename", &json!({ "table": "layer", "from": name, "to": to })).map_err(fail)?;
        }
        let name = renamed.unwrap_or_else(|| name.to_string());
        let doc = self.session.doc_mut().map_err(fail)?;
        let Some(layer) = doc.layers.iter_mut().find(|l| l.name == name) else { return Err(fail(format!("there's no layer {name}"))) };
        let Value::Object(mut fields) = serde_json::to_value(&*layer).map_err(fail)? else { return Ok(()) };
        fields.extend(patch.into_iter().filter(|(k, _)| k != "name"));
        *layer = serde_json::from_value(Value::Object(fields)).map_err(fail)?;
        Ok(())
    }

    /// The AutoCAD Color Index's 256 colours as `0xRRGGBB` (index 0 and 256 aren't colours: black).
    pub fn palette() -> Vec<u32> {
        (0..=255u8)
            .map(|i| if i == 0 { 0 } else { cadcraft_engine::doc::color::Color::Index(i).resolve(Default::default(), Default::default()).to_u32() })
            .collect()
    }

    /// An object's key in the document (its handle in hex).
    pub fn key(handle: f64) -> String {
        hex(Handle(handle as u64))
    }

    /// Every command, as JSON `CommandInfo`s (id, label, menu, shortcut, aliases).
    pub fn commands(&self) -> String {
        let all: Vec<_> = cadcraft_engine::command_specs().iter().map(|c| c.info(&self.session)).collect();
        serde_json::to_string(&all).unwrap_or_default()
    }

    /// The space shown (model, or a layout's name) as a display list. `tolerance` is the chord error in drawing units
    /// (about half a pixel), `dash` two pixels. Model space is built a chunk of objects at a time, and only the chunks an
    /// edit touched are built again.
    pub fn scene(&mut self, tolerance: f64, dash: f64) -> Result<Scene, JsError> {
        let st = self.session.state().map_err(fail)?;
        let doc = st.doc.clone();
        let opts = Options { tolerance: tolerance.max(1e-9), min_dash: dash.max(0.0), ..Options::default() };
        if st.space != Space::Model {
            let list = render::build(&doc, &st.space, &opts);
            return Ok(Scene::of(&[&list], self.cache.origin));
        }
        let c = &mut self.cache;
        let fresh = c.tolerance != opts.tolerance || c.dash != opts.min_dash || c.doc.as_ref().is_none_or(|d| !same_look(d, &doc));
        if fresh {
            c.lists.clear();
            c.tolerance = opts.tolerance;
            c.dash = opts.min_dash;
        }
        let mut kept = std::collections::HashMap::with_capacity(c.lists.len());
        for (key, chunk) in doc.model.chunk_slices() {
            let list = c.lists.remove(&key).unwrap_or_else(|| render::build_entities(&doc, chunk.iter().map(|e| &**e), &opts));
            kept.insert(key, list);
        }
        c.lists = kept;
        let lists: Vec<&render::DisplayList> = doc.model.chunk_slices().filter_map(|(key, _)| c.lists.get(&key)).collect();
        let scene = Scene::of(&lists, c.origin);
        c.origin.get_or_insert(scene.origin_xy());
        c.doc = Some(doc);
        Ok(scene)
    }

    /// What the command running draws under the cursor (its rubber band), relative to `origin` (`[x, y]`) and, when
    /// it drags objects (MOVE, ROTATE, a grip), their handles and the matrix moving them: JSON `{handles, matrix}` from
    /// `drag`.
    pub fn preview(&self, x: f64, y: f64, tolerance: f64, origin: Vec<f64>) -> Result<Scene, JsError> {
        let d = self.session.doc().map_err(fail)?;
        let cursor = Vec2::new(x, y);
        let mut ents = self.session.preview(cursor);
        if let Some((_, extra)) = self.session.drag(cursor) {
            ents.extend(extra);
        }
        let opts = Options { tolerance: tolerance.max(1e-9), ..Options::default() };
        let list = render::build_entities(d, ents.iter(), &opts);
        let origin = match origin.as_slice() {
            [x, y] => Some([*x, *y]),
            _ => None,
        };
        Ok(Scene::of(&[&list], origin))
    }

    /// The objects the command running drags at `x, y`, and how: JSON `{handles, matrix: [a, b, c, d, e, f]}` (x' = a x
    /// + c y + e, y' = b x + d y + f), or "" when it drags none.
    pub fn drag(&self, x: f64, y: f64) -> String {
        let Some((d, _)) = self.session.drag(Vec2::new(x, y)) else { return String::new() };
        if d.clipboard || d.handles.is_empty() {
            return String::new();
        }
        let m = d.matrix;
        let a = m.apply(Vec2::new(1.0, 0.0)) - m.apply(Vec2::ZERO);
        let c = m.apply(Vec2::new(0.0, 1.0)) - m.apply(Vec2::ZERO);
        let e = m.apply(Vec2::ZERO);
        json!({ "handles": d.handles.iter().map(|h| h.0).collect::<Vec<_>>(), "matrix": [a.x, a.y, c.x, c.y, e.x, e.y] }).to_string()
    }
}

/// Is this file a DWG (which the app's native side turns into DXF first)?
#[wasm_bindgen]
pub fn is_dwg(bytes: &[u8]) -> bool {
    bytes.starts_with(b"AC10") || bytes.starts_with(b"AC1.")
}
