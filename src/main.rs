//! Memrium Icons: an icon composer. An icon of your applications (a Papirus one, say) is opened as a stack of layers on a
//! plate. Every layer and group has its colour, opacity and blend mode, its Liquid Glass (shadow, frosting, bending,
//! translucency, edge light) and its place; the plate has a shape and colours; and there is a look for a dark theme and a
//! single-colour one. Documents are saved in the `.memicon` format (`memrium_de::icondoc`); "Build icon theme" writes them as
//! an icon theme that every program can use. Drawing is `memrium_de::iconrender`.
//!
//!   memrium_icons [icon-name]

use chebrium::app::{self, App, Config, Host, KeyEvent, Keysym, Modifiers};
use chebrium::appmenu::{Item, Menu};
use chebrium::draw::Align;
use chebrium::form::Form;
use chebrium::input::KeyMods;
use chebrium::picture::Pic;
use chebrium::ui::{inside, Cursor, Ui};
use memrium_de::apps::{self, IconIndex};
use memrium_de::colour::{hsv_to_rgb, parse_hex, rgb_to_hsv, to_hex};
use memrium_de::icondoc::{self, Appearance, Background, Blend, Drop, FillKind, Glass, GlassMode, Gradient, IconDoc, Kind, Props, ShadowKind, Shape};
use memrium_de::iconrender::{self, import_svg, GlassTune, Renderer, SvgOp, GLASS_PRESETS};
use memrium_de::wallpapers;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{channel, Receiver, Sender},
    Arc,
};

const LEFT_W: f32 = 270.0;
const RIGHT_W: f32 = 370.0;
const BAR_H: f32 = 60.0;
/// the height of the menu bar the program draws itself, when the shell has none to show its menus
const MENU_H: f32 = 30.0;
const ROW_H: f32 = 30.0;
/// size of the picture of the icon in the middle
const BIG: u32 = 320;
const SOURCE_THEME: &str = "Papirus";
/// The photo the stage starts on.
const PREFERRED_WALLPAPER: &str = "Scarlet Tree";

struct Entry {
    name: String,
    icon: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Sel {
    Background,
    Node(u32),
}

#[derive(Clone, Copy, PartialEq)]
enum PickTarget {
    BgTop,
    BgBottom,
    LayerFill,
    GradFrom,
    GradTo,
    TintColour,
}

/// The colour picker popup: a square for saturation and value, a hue bar, a hex field and an eyedropper.
struct Picker {
    target: PickTarget,
    hsv: [f32; 3],
    hex: String,
    /// what is being dragged: 1 the square, 2 the hue bar
    drag: u8,
    /// the next click on the picture takes its colour
    eyedrop: bool,
    /// where on the window the swatch that opened it was
    anchor_y: f32,
    /// opened in this very frame: the click that opened it must not close it again
    fresh: bool,
    /// the hex field has the keyboard, and its whole text is selected (typing replaces it)
    focus: bool,
    selected: bool,
    /// the colour the document was last given: opening the picker changes nothing until the colour does
    applied: [f32; 3],
}

/// The icon drawn by the worker thread.
struct Job {
    version: u64,
    look: Appearance,
    doc: IconDoc,
    flat: bool,
    n: u32,
    tune: GlassTune,
    backdrop: Option<Arc<image::RgbaImage>>,
    /// the grid: what is behind each of the six looks
    behind: Vec<Arc<image::RgbaImage>>,
    /// all six looks at the size of the overview, not the one look being edited
    grid: bool,
}

enum Done {
    One { version: u64, look: Appearance, img: image::RgbaImage, plain: Option<image::RgbaImage> },
    Grid { version: u64, imgs: Vec<(Appearance, image::RgbaImage)> },
}

/// The overview shows the looks in this order: the light ones above, the dark ones below.
const GRID_ORDER: [Appearance; 6] = [Appearance::Default, Appearance::ClearLight, Appearance::TintedLight, Appearance::Dark, Appearance::ClearDark, Appearance::TintedDark];
/// the size of an icon in the overview
const GRID_N: u32 = 180;

/// The picture of the icon on the stage and its smaller copies.
struct Shown {
    version: u64,
    look: Appearance,
    img: image::RgbaImage,
    big: Arc<Pic>,
    small: Vec<(u32, Arc<Pic>)>,
}

enum Msg {
    Progress(usize, usize),
    Done(Result<usize, String>),
}

struct Build {
    rx: Receiver<Msg>,
    cancel: Arc<AtomicBool>,
    done: usize,
    total: usize,
}

/// The ids of `nodes` and what is in them, as the list shows them: the front first.
fn front_to_back(nodes: &[icondoc::Node], out: &mut Vec<u32>) {
    for n in nodes.iter().rev() {
        out.push(n.id);
        front_to_back(n.children(), out);
    }
}

/// Moving or scaling the selected layer on the icon.
struct CanvasDrag {
    id: u32,
    scale: bool,
    /// where the pointer was when it began, and the layer's settings then
    from: [f32; 2],
    props: Props,
    /// the other picked layers, which move and scale along
    others: Vec<(u32, Props)>,
}

/// A layer being dragged in the list.
struct Drag {
    id: u32,
    from: [f32; 2],
    /// the pointer has moved far enough for it to be a drag and not a click
    active: bool,
}

struct Row {
    id: u32,
    depth: usize,
    name: String,
    group: bool,
    open: bool,
    shown: bool,
}

struct Composer {
    entries: Vec<Entry>,
    query: String,
    /// 0 the layers of the icon, 1 the applications to pick from
    left_tab: usize,
    list_scroll: f32,
    tree_scroll: f32,
    insp_scroll: f32,
    insp_height: f32,
    index: IconIndex,
    sources: HashMap<String, Option<Arc<Pic>>>,
    doc: Option<IconDoc>,
    sel: Option<Sel>,
    /// further layers picked with Shift or Ctrl, besides the one in `sel`
    also: Vec<u32>,
    collapsed: HashSet<u32>,
    look: Appearance,
    backdrop: usize,
    undo: Vec<IconDoc>,
    redo: Vec<IconDoc>,
    /// the document as it was when the pointer went down: what Undo goes back to if the press changed something
    pending: Option<IconDoc>,
    /// draws on this thread: layer pictures for the list, hit tests, outlines
    drawer: Renderer,
    jobs: Option<Sender<Job>>,
    results: Option<Receiver<Done>>,
    /// grows with every change of the document
    version: u64,
    sent: Option<(u64, Appearance)>,
    shown: Option<Shown>,
    outline: Option<(u32, u64, Appearance, Arc<Pic>)>,
    thumbs: HashMap<u32, (u64, Arc<Pic>)>,
    picker: Option<Picker>,
    big_rect: [f32; 4],
    rows: Vec<(u32, [f32; 4])>,
    drag: Option<Drag>,
    /// 0 the icon being edited, 1 all six looks side by side
    view: usize,
    grid: Option<(u64, Vec<(Appearance, Arc<Pic>)>)>,
    grid_sent: Option<u64>,
    /// the flat version is shown: very plain glass
    flat: bool,
    /// the global strength of all glass
    tune: GlassTune,
    /// the wallpapers to try the icon on: the list, the one shown and its picture
    walls: Vec<(String, std::path::PathBuf)>,
    wall: usize,
    wall_pic: Option<(usize, Arc<Pic>, f32)>,
    wall_img: Option<Arc<image::RgbaImage>>,
    /// the wallpaper blurred, for the panels that lie over it
    wall_blur: Option<(usize, Arc<Pic>)>,
    /// the whole window
    win: [f32; 4],
    dial_drag: bool,
    grid_behind: Option<Vec<Arc<image::RgbaImage>>>,
    grid_key: Option<(usize, [i32; 4], [i32; 4])>,
    /// where the backdrop of the icon was cut from: the wallpaper, the icon's place and the stage
    backdrop_key: Option<(usize, [i32; 4], [i32; 4])>,
    behind: Option<Arc<image::RgbaImage>>,
    /// the menu of the program's own menu bar that is open, and the submenus open in it (an index at each depth)
    menu_open: Option<usize>,
    menu_sub: Vec<usize>,
    /// the window with the glass sliders: shown, where its top left is, and where it was grabbed
    glass_panel: bool,
    glass_pos: [f32; 2],
    glass_grab: Option<[f32; 2]>,
    /// the `.icon` folders of Apple's Icon Composer found on the Desktop and in Downloads
    apple_icons: Vec<std::path::PathBuf>,
    /// how big the icon is on the stage (1 is `BIG` pixels)
    zoom: f32,
    /// every change is written into the icon theme at once, and the shell shows it
    live: bool,
    live_busy: Arc<AtomicBool>,
    live_pending: bool,
    live_all_due: Option<std::time::Instant>,
    /// "Reset all icons" was pressed once and waits for a second press
    confirm_reset_all: bool,
    grid_rects: Vec<(Appearance, [f32; 4])>,
    cdrag: Option<CanvasDrag>,
    /// the box round the selection on the icon, relative to the icon's top left
    sel_box: Option<[f32; 4]>,
    save_due: bool,
    build: Option<Build>,
    note: String,
    waker: Option<Arc<app::Waker>>,
}

fn page_color(dark: bool) -> [f32; 3] {
    if dark { [0.115, 0.115, 0.125] } else { [0.955, 0.960, 0.972] }
}

fn card_color(dark: bool) -> [f32; 3] {
    if dark { [0.165, 0.165, 0.178] } else { [1.0, 1.0, 1.0] }
}

fn fit(ui: &mut Ui<'_>, text: &str, size: f32, bold: bool, max: f32) -> String {
    if ui.measure_raw(text, size, bold) <= max {
        return text.to_string();
    }
    let mut s = text.to_string();
    while !s.is_empty() && ui.measure_raw(&format!("{s}…"), size, bold) > max {
        s.pop();
    }
    format!("{s}…")
}

/// The outline of a drawn shape (its alpha), in `colour`, with a faint wash inside: the picture that marks the selection.
fn outline_of(mask: &image::RgbaImage, colour: [f32; 3]) -> image::RgbaImage {
    let (w, h) = (mask.width() as usize, mask.height() as usize);
    let a: Vec<f32> = mask.pixels().map(|p| p[3] as f32 / 255.0).collect();
    // grey-scale dilation and erosion by 2 pixels, one direction at a time; their difference is the edge
    let pass = |src: &[f32], horizontal: bool, grow: bool| -> Vec<f32> {
        let mut out = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let mut v = if grow { 0.0f32 } else { 1.0 };
                for d in -2i32..=2 {
                    let (xx, yy) = if horizontal { ((x as i32 + d).clamp(0, w as i32 - 1) as usize, y) } else { (x, (y as i32 + d).clamp(0, h as i32 - 1) as usize) };
                    let s = src[yy * w + xx];
                    v = if grow { v.max(s) } else { v.min(s) };
                }
                out[y * w + x] = v;
            }
        }
        out
    };
    let dil = pass(&pass(&a, true, true), false, true);
    let ero = pass(&pass(&a, true, false), false, false);
    let mut out = image::RgbaImage::new(w as u32, h as u32);
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let alpha = ((dil[i] - ero[i]) * 1.4).max(a[i] * 0.12).clamp(0.0, 1.0);
            if alpha > 0.0 {
                out.put_pixel(x as u32, y as u32, image::Rgba([(colour[0] * 255.0) as u8, (colour[1] * 255.0) as u8, (colour[2] * 255.0) as u8, (alpha * 255.0) as u8]));
            }
        }
    }
    out
}

/// A section title with a glyph before it.
fn section_icon(ui: &mut Ui<'_>, x: f32, y: f32, glyph: &str, label: &str) {
    ui.glyph(x + 7.0, y - 1.0, 14.0, glyph, ui.ink, 0.55);
    ui.section(x + 20.0, y, label);
}

/// A small "Reset" link with an arrow, right-aligned at `x_right`; true when it was pressed.
fn reset_link(ui: &mut Ui<'_>, x_right: f32, y: f32, label: &str) -> bool {
    let w = ui.measure(label, 11.5, true) + 20.0;
    let r = [x_right - w, y - 14.0, w, 20.0];
    let hov = ui.hit(r);
    let accent = ui.accent;
    ui.glyph(r[0] + 7.0, y - 4.5, 12.0, "rotate-ccw", accent, if hov { 0.95 } else { 0.6 });
    ui.text(r[0] + 16.0, y, label, 11.5, true, accent, if hov { 0.95 } else { 0.6 }, Align::Left);
    if hov {
        ui.want_cursor(Cursor::Pointer);
    }
    hov && ui.click
}

/// A row with a label and a swatch of `colour` (with its hex code) that opens the colour picker; true when pressed.
/// `None` is shown as "Original colours".
fn swatch(ui: &mut Ui<'_>, f: &mut Form, label: &str, colour: Option<[f32; 3]>) -> bool {
    ui.row(f.x, f.y, label);
    let r = [f.x + 110.0, f.y - 15.0, f.cw - 110.0, 30.0];
    let hov = ui.hit(r);
    let ink = ui.ink;
    ui.fill(r, 9.0, ink, if hov { 0.35 } else { 0.2 });
    match colour {
        Some(c) => {
            ui.fill([r[0] + 1.0, r[1] + 1.0, r[2] - 2.0, r[3] - 2.0], 8.0, c, 1.0);
            let light = 0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2] > 0.6;
            let on = if light { [0.08; 3] } else { [1.0; 3] };
            ui.text_raw(r[0] + 12.0, r[1] + 20.0, &to_hex(c), 12.5, true, on, 0.9, Align::Left);
            ui.glyph(r[0] + r[2] - 18.0, r[1] + 15.0, 14.0, "pipette", on, 0.7);
        }
        None => {
            ui.fill([r[0] + 1.0, r[1] + 1.0, r[2] - 2.0, r[3] - 2.0], 8.0, card_color(ui.dark), 1.0);
            ui.text(r[0] + 12.0, r[1] + 20.0, "Original colours", 12.5, false, ink, 0.6, Align::Left);
            ui.glyph(r[0] + r[2] - 18.0, r[1] + 15.0, 14.0, "pipette", ink, 0.5);
        }
    }
    if hov {
        ui.want_cursor(Cursor::Pointer);
    }
    f.skip(36.0);
    hov && ui.click
}

/// A row of one choice out of `all`, drawn as a segmented control with the label on its left.
fn pick<T: Copy + PartialEq>(ui: &mut Ui<'_>, f: &mut Form, label: &str, value: &mut T, all: &[T], name: fn(T) -> &'static str) -> bool {
    ui.row(f.x, f.y, label);
    let names: Vec<&str> = all.iter().map(|v| name(*v)).collect();
    let mut at = all.iter().position(|v| v == value).unwrap_or(0);
    let changed = ui.segmented([f.x + 110.0, f.y - 15.0, f.cw - 110.0, 30.0], &names, &mut at);
    if changed {
        *value = all[at];
    }
    f.skip(36.0);
    changed
}

fn percent(ui: &mut Ui<'_>, f: &mut Form, label: &str, v: &mut f32) {
    let mut x = *v * 100.0;
    f.slider(ui, label, &mut x, 0.0, 100.0, "%");
    *v = x / 100.0;
}

/// The Liquid Glass rows of a layer, group or plate.
fn glass_rows(ui: &mut Ui<'_>, f: &mut Form, g: &mut Glass) {
    f.toggle(ui, "Liquid Glass", &mut g.enabled);
    if !g.enabled {
        return;
    }
    percent(ui, f, "Specular", &mut g.specular);
    percent(ui, f, "Blur", &mut g.blur);
    percent(ui, f, "Refractivity", &mut g.refraction);
    percent(ui, f, "Translucency", &mut g.translucency);
    percent(ui, f, "Bevel", &mut g.bevel);
    if g.bevel > 0.0 {
        percent(ui, f, "Bevel width", &mut g.bevel_width);
    }
    percent(ui, f, "Inner shadow", &mut g.inner_shadow);
    percent(ui, f, "Glow", &mut g.glow);
    pick(ui, f, "Shadow", &mut g.shadow, ShadowKind::ALL, ShadowKind::label);
    if g.shadow != ShadowKind::None {
        percent(ui, f, "Shadow strength", &mut g.shadow_amount);
    }
}

fn hash_of(parts: &[&dyn Fn(&mut std::collections::hash_map::DefaultHasher)]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for p in parts {
        p(&mut h);
    }
    h.finish()
}

impl Composer {
    fn new(first: Option<String>) -> Composer {
        let mut entries: Vec<Entry> = Vec::new();
        for a in apps::scan() {
            if a.icon.is_empty() || a.icon.starts_with('/') || entries.iter().any(|e| e.icon == a.icon) {
                continue;
            }
            entries.push(Entry { name: a.name, icon: a.icon });
        }
        entries.sort_by_key(|e| e.name.to_lowercase());
        let home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default());
        let apple_icons = memrium_de::appleicon::find_icons(&[home.join("Desktop"), home.join("Downloads")]);
        let opened_apple = first.as_deref().map(std::path::Path::new).filter(|p| memrium_de::appleicon::is_icon_dir(p)).map(|p| p.to_path_buf());
        let first = first.filter(|f| opened_apple.is_none() || f.is_empty()).or_else(|| icondoc::saved_names().into_iter().next()).or_else(|| entries.first().map(|e| e.icon.clone()));
        let mut c = Composer {
            entries,
            query: String::new(),
            left_tab: 0,
            list_scroll: 0.0,
            tree_scroll: 0.0,
            insp_scroll: 0.0,
            insp_height: 0.0,
            index: IconIndex::build_pack(SOURCE_THEME),
            sources: HashMap::new(),
            doc: None,
            sel: None,
            also: Vec::new(),
            collapsed: HashSet::new(),
            look: Appearance::Default,
            backdrop: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            pending: None,
            drawer: Renderer::new(),
            jobs: None,
            results: None,
            version: 0,
            sent: None,
            shown: None,
            outline: None,
            thumbs: HashMap::new(),
            picker: None,
            big_rect: [0.0; 4],
            rows: Vec::new(),
            drag: None,
            view: 0,
            grid: None,
            grid_sent: None,
            flat: false,
            tune: GlassTune::load(),
            walls: Vec::new(),
            wall: 0,
            wall_pic: None,
            zoom: 1.0,
            apple_icons: Vec::new(),
            glass_panel: false,
            glass_pos: [-1.0, 70.0],
            glass_grab: None,
            menu_open: None,
            menu_sub: Vec::new(),
            live: true,
            live_busy: Arc::new(AtomicBool::new(false)),
            live_pending: false,
            live_all_due: None,
            confirm_reset_all: false,
            wall_img: None,
            wall_blur: None,
            win: [0.0; 4],
            dial_drag: false,
            grid_behind: None,
            grid_key: None,
            backdrop_key: None,
            behind: None,
            grid_rects: Vec::new(),
            cdrag: None,
            sel_box: None,
            save_due: false,
            build: None,
            note: String::new(),
            waker: None,
        };
        if let Some(name) = first {
            c.open(&name);
        }
        if let Some(p) = opened_apple {
            c.import_apple(&p);
        }
        c
    }

    /// Make a document of an Apple `.icon` folder and open it (what was open stays in Undo).
    fn import_apple(&mut self, dir: &std::path::Path) {
        match memrium_de::appleicon::import(dir) {
            Ok(doc) => {
                self.flush();
                if let Some(old) = self.doc.take() {
                    self.undo.push(old);
                    self.redo.clear();
                }
                self.note = format!("Imported {} from Apple's Icon Composer.", doc.name);
                self.doc = Some(doc);
                self.sel = None;
                self.also.clear();
                self.look = Appearance::Default;
                self.touch();
            }
            Err(e) => self.note = format!("Cannot import {}: {e}", dir.display()),
        }
    }

    // ------------------------------------------------------------------------------------------------ the document

    /// Open the document of icon `name`: the saved one, or a new one made from the icon's SVG.
    fn open(&mut self, name: &str) {
        self.flush();
        let path = icondoc::doc_path(name);
        let doc = if path.exists() {
            icondoc::load(&path).map_err(|e| format!("Cannot open {name}: {e}"))
        } else {
            match self.index.find(name, 128) {
                Some(p) if p.extension().is_some_and(|e| e == "svg") => std::fs::read_to_string(&p).map_err(|e| e.to_string()).and_then(|t| import_svg(name, &t)),
                Some(_) => Err("This icon is a PNG, so it has no shapes to make layers of.".to_string()),
                None => Err(format!("No icon called {name} was found.")),
            }
        };
        match doc {
            Ok(d) => {
                self.doc = Some(d);
                self.note.clear();
            }
            Err(e) => {
                self.note = e;
                return;
            }
        }
        self.sel = None;
        self.also.clear();
        self.collapsed.clear();
        self.undo.clear();
        self.redo.clear();
        self.pending = None;
        self.shown = None;
        self.grid = None;
        self.grid_sent = None;
        self.outline = None;
        self.thumbs.clear();
        self.picker = None;
        self.tree_scroll = 0.0;
        self.insp_scroll = 0.0;
        self.version += 1;
        self.sent = None;
    }

    /// The document changed.
    /// The size in pixels of the icon drawn on the stage.
    fn n(&self) -> u32 {
        ((BIG as f32 * self.zoom).round() as u32).clamp(160, 640)
    }

    /// New glass settings for every icon: kept, drawn again, and written into the theme a moment after the last change.
    fn apply_tune(&mut self, t: GlassTune) {
        if t == self.tune {
            return;
        }
        self.tune = t;
        t.save();
        self.version += 1;
        self.thumbs.clear();
        self.outline = None;
        self.live_all_due = Some(std::time::Instant::now());
    }

    fn set_zoom(&mut self, z: f32) {
        let z = (z * 20.0).round() / 20.0;
        let z = z.clamp(0.5, 2.0);
        if (z - self.zoom).abs() > 1e-4 {
            self.zoom = z;
            // the picture is drawn again at the new size
            self.version += 1;
            self.outline = None;
            self.thumbs.clear();
        }
    }

    fn touch(&mut self) {
        self.version += 1;
        self.save_due = true;
        self.outline = None;
    }

    /// Write the document if it changed.
    fn flush(&mut self) {
        if !self.save_due {
            return;
        }
        self.save_due = false;
        if let Some(d) = &self.doc {
            if let Err(e) = icondoc::save(&icondoc::doc_path(&d.name), d) {
                self.note = format!("Could not save: {e}");
            }
        }
        self.live_write(false);
    }

    /// Every picked layer: the main one first, then the others.
    fn picked(&self) -> Vec<u32> {
        let mut v = Vec::new();
        if let Some(Sel::Node(id)) = self.sel {
            v.push(id);
        }
        v.extend(self.also.iter().copied());
        v
    }

    fn prune_picked(&mut self) {
        if let Some(d) = &self.doc {
            self.also.retain(|id| d.find(*id).is_some());
        }
    }

    /// Pick layer `id`: plainly, or added to / taken out of the picked ones with Ctrl or Shift. `order` is the list as it is
    /// shown, for Shift to take the stretch up to `id`.
    fn pick(&mut self, id: u32, mods: KeyMods, order: &[u32]) {
        let main = match self.sel {
            Some(Sel::Node(m)) => Some(m),
            _ => None,
        };
        match main {
            Some(m) if mods.shift && m != id => {
                let (a, b) = (order.iter().position(|&x| x == m), order.iter().position(|&x| x == id));
                if let (Some(a), Some(b)) = (a, b) {
                    self.also = order[a.min(b)..=a.max(b)].iter().copied().filter(|&x| x != m).collect();
                }
            }
            Some(m) if mods.ctrl && m != id => {
                if let Some(i) = self.also.iter().position(|&x| x == id) {
                    self.also.remove(i);
                } else {
                    self.also.push(id);
                }
            }
            Some(m) if mods.ctrl && m == id => {
                // the main one is taken out: another picked layer takes over
                if self.also.is_empty() {
                    self.sel = None;
                } else {
                    self.sel = Some(Sel::Node(self.also.remove(0)));
                }
            }
            _ => {
                self.also.clear();
                self.sel = Some(Sel::Node(id));
            }
        }
    }

    fn props_of(&self, id: u32) -> Option<Props> {
        self.doc.as_ref()?.find(id).map(|n| iconrender::effective_props(n, self.look))
    }

    /// Change the settings of node `id` in the look being edited.
    fn set_props(&mut self, id: u32, p: Props) {
        let look = self.look;
        let mut changed = false;
        if let Some(n) = self.doc.as_mut().and_then(|d| d.find_mut(id)) {
            if iconrender::effective_props(n, look) != p {
                iconrender::set_node_props(n, look, p);
                changed = true;
            }
        }
        if changed {
            self.touch();
        }
    }

    fn set_background(&mut self, b: Background) {
        let look = self.look;
        let mut changed = false;
        if let Some(d) = self.doc.as_mut() {
            if iconrender::background_for(d, look) != b {
                iconrender::set_background(d, look, b);
                changed = true;
            }
        }
        if changed {
            self.touch();
        }
    }

    fn undo_step(&mut self) {
        if let Some(prev) = self.undo.pop() {
            if let Some(now) = self.doc.take() {
                self.redo.push(now);
            }
            self.doc = Some(prev);
            self.after_history();
        }
    }

    fn redo_step(&mut self) {
        if let Some(next) = self.redo.pop() {
            if let Some(now) = self.doc.take() {
                self.undo.push(now);
            }
            self.doc = Some(next);
            self.after_history();
        }
    }

    fn after_history(&mut self) {
        // the selected layer may be gone
        if let (Some(Sel::Node(id)), Some(d)) = (self.sel, &self.doc) {
            if d.find(id).is_none() {
                self.sel = None;
            }
        }
        self.prune_picked();
        self.touch();
    }

    /// Rows of the layer list, front to back.
    fn rows_of(&self) -> Vec<Row> {
        fn walk(nodes: &[icondoc::Node], depth: usize, look: Appearance, collapsed: &HashSet<u32>, out: &mut Vec<Row>) {
            for n in nodes.iter().rev() {
                let group = matches!(n.kind, Kind::Group(_));
                let open = !collapsed.contains(&n.id);
                out.push(Row { id: n.id, depth, name: n.name.clone(), group, open, shown: iconrender::effective_props(n, look).visible });
                if group && open {
                    walk(n.children(), depth + 1, look, collapsed, out);
                }
            }
        }
        let mut out = Vec::new();
        if let Some(d) = &self.doc {
            walk(&d.nodes, 0, self.look, &self.collapsed, &mut out);
        }
        out
    }

    // ------------------------------------------------------------------------------------------------ drawing the icon

    /// Start drawing the icon again when the document or the look changed: on the worker thread if there is one.
    fn request_render(&mut self) {
        let Some(doc) = self.doc.clone() else { return };
        if self.sent != Some((self.version, self.look)) {
            self.sent = Some((self.version, self.look));
            match &self.jobs {
                Some(tx) => {
                    let _ = tx.send(Job { version: self.version, look: self.look, doc: doc.clone(), flat: self.flat, n: self.n(), tune: self.tune, backdrop: self.behind.clone(), behind: Vec::new(), grid: false });
                }
                None => {
                    let mut r = Renderer::new();
                    r.flat = self.flat;
                    r.tune = self.tune;
                    r.backdrop = self.behind.clone();
                    let img = r.render(&doc, self.look, self.n());
                    let plain = r.backdrop.take().map(|_| r.render(&doc, self.look, 128));
                    self.accept(Done::One { version: self.version, look: self.look, img, plain });
                }
            }
        }
        // the overview is only drawn while it is shown
        if self.view == 1 && self.grid_sent != Some(self.version) {
            self.grid_sent = Some(self.version);
            match &self.jobs {
                Some(tx) => {
                    let _ = tx.send(Job { version: self.version, look: self.look, doc, flat: self.flat, n: GRID_N, tune: self.tune, backdrop: None, behind: self.grid_behind.clone().unwrap_or_default(), grid: true });
                }
                None => {
                    let mut r = Renderer::new();
                    r.flat = self.flat;
                    r.tune = self.tune;
                    let behind = self.grid_behind.clone().unwrap_or_default();
                    let imgs = GRID_ORDER.iter().enumerate().map(|(k, l)| {
                        r.backdrop = behind.get(k).cloned();
                        (*l, r.render(&doc, *l, GRID_N))
                    }).collect();
                    self.accept(Done::Grid { version: self.version, imgs });
                }
            }
        }
    }

    fn accept(&mut self, d: Done) {
        let pic = |img: &image::RgbaImage| Arc::new(Pic::from_rgba(img.width(), img.height(), img.as_raw()));
        match d {
            Done::One { version, look, img, plain } => {
                // a picture of an older document is shown until the newer one arrives, but not after a newer one has been shown
                if self.shown.as_ref().is_some_and(|s| s.version > version) {
                    return;
                }
                let from = plain.as_ref().unwrap_or(&img);
                let small = [128u32, 64, 48, 32].iter().map(|&n| (n, pic(&image::imageops::resize(from, n, n, image::imageops::FilterType::Lanczos3)))).collect();
                self.shown = Some(Shown { version, look, big: pic(&img), img, small });
            }
            Done::Grid { version, imgs } => {
                if self.grid.as_ref().is_some_and(|g| g.0 > version) {
                    return;
                }
                self.grid = Some((version, imgs.iter().map(|(l, img)| (*l, pic(img))).collect()));
            }
        }
    }

    fn poll(&mut self) {
        let (mut one, mut grid) = (None, None);
        if let Some(rx) = &self.results {
            while let Ok(d) = rx.try_recv() {
                match d {
                    Done::One { .. } => one = Some(d),
                    Done::Grid { .. } => grid = Some(d),
                }
            }
        }
        for d in [one, grid].into_iter().flatten() {
            self.accept(d);
        }
        self.request_render();
    }

    /// The small picture of a layer for the list.
    fn thumb(&mut self, id: u32) -> Option<Arc<Pic>> {
        let doc = self.doc.as_ref()?;
        let node = doc.find(id)?;
        let Kind::Layer(layer) = &node.kind else { return None };
        let p = iconrender::effective_props(node, self.look);
        let look = self.look;
        let key = hash_of(&[&|h| layer.svg.hash(h), &|h| p.fill.map(|c| c.map(|v| v.to_bits())).hash(h), &|h| look.slot().hash(h)]);
        if let Some((k, pic)) = self.thumbs.get(&id) {
            if *k == key {
                return Some(pic.clone());
            }
        }
        let img = self.drawer.raster(layer, &p, doc, self.look, 48);
        let pic = Arc::new(Pic::from_rgba(48, 48, img.as_raw()));
        self.thumbs.insert(id, (key, pic.clone()));
        Some(pic)
    }

    fn source_pic(&mut self, icon: &str) -> Option<Arc<Pic>> {
        if let Some(p) = self.sources.get(icon) {
            return p.clone();
        }
        let pic = self.index.load(icon, 64).map(|img| Arc::new(Pic::from_rgba(64, 64, img.as_raw())));
        self.sources.insert(icon.to_string(), pic.clone());
        pic
    }

    // ------------------------------------------------------------------------------------------------ the left panel

    fn draw_left(&mut self, ui: &mut Ui<'_>, area: [f32; 4]) {
        let ink = ui.ink;
        self.panel_bg(ui, area);
        ui.fill([area[0] + area[2] - 1.0, area[1], 1.0, area[3]], 0.0, ink, 0.09);
        let mut tab = self.left_tab;
        if ui.segmented([area[0] + 14.0, area[1] + 14.0, area[2] - 28.0, 30.0], &["Layers", "Icons"], &mut tab) {
            self.left_tab = tab;
        }
        if self.left_tab == 0 {
            self.draw_tree(ui, [area[0], area[1] + 54.0, area[2] - 1.0, area[3] - 54.0]);
        } else {
            self.draw_apps(ui, [area[0], area[1] + 54.0, area[2] - 1.0, area[3] - 54.0]);
        }
    }

    fn draw_apps(&mut self, ui: &mut Ui<'_>, area: [f32; 4]) {
        let (ink, accent) = (ui.ink, ui.accent);
        let mut q = self.query.clone();
        ui.push_clip([area[0] + 9.0, area[1] - 3.0, area[2] - 18.0, 36.0]);
        ui.text_field([area[0] + 12.0, area[1], area[2] - 24.0, 30.0], "search", &mut q);
        ui.pop_clip();
        if q.is_empty() {
            ui.text(area[0] + 24.0, area[1] + 20.0, "Search applications", 12.5, false, ink, 0.4, Align::Left);
        }
        self.query = q;
        let list = [area[0], area[1] + 40.0, area[2], area[3] - 40.0];
        let ql = self.query.trim().to_lowercase();
        let rows: Vec<usize> = (0..self.entries.len()).filter(|&i| ql.is_empty() || self.entries[i].name.to_lowercase().contains(&ql) || self.entries[i].icon.to_lowercase().contains(&ql)).collect();
        const H: f32 = 44.0;
        let max_scroll = (rows.len() as f32 * H + 8.0 - list[3]).max(0.0);
        if ui.scroll != 0.0 && inside(ui.mouse, list) {
            self.list_scroll += ui.scroll;
        }
        self.list_scroll = self.list_scroll.clamp(0.0, max_scroll);
        let scroll = ui.ease(9_001, self.list_scroll, 18.0);
        let saved: HashSet<String> = icondoc::saved_names().into_iter().collect();
        let current = self.doc.as_ref().map(|d| d.name.clone());
        ui.push_clip(list);
        let first = (scroll / H).floor().max(0.0) as usize;
        let last = (((scroll + list[3]) / H).ceil() as usize + 1).min(rows.len());
        let mut open = None;
        for (k, &i) in rows.iter().enumerate().take(last).skip(first) {
            let (name, icon) = (self.entries[i].name.clone(), self.entries[i].icon.clone());
            let r = [list[0] + 6.0, list[1] - scroll + 4.0 + k as f32 * H, list[2] - 12.0, H - 2.0];
            let on = current.as_deref() == Some(icon.as_str());
            let hov = ui.hit(r) && inside(ui.mouse, list);
            if on {
                ui.fill(r, 10.0, accent, 0.20);
            } else if hov {
                ui.fill(r, 10.0, ink, 0.06);
            }
            if let Some(pic) = self.source_pic(&icon) {
                ui.picture([r[0] + 8.0, r[1] + 5.0, 32.0, 32.0], &pic, [0.0, 0.0, 64.0, 64.0], 1.0);
            }
            let label = fit(ui, &name, 13.0, on, r[2] - 70.0);
            ui.text_raw(r[0] + 50.0, r[1] + 26.0, &label, 13.0, on, ink, 0.92, Align::Left);
            if saved.contains(&icon) {
                ui.fill([r[0] + r[2] - 18.0, r[1] + r[3] / 2.0 - 3.0, 6.0, 6.0], 3.0, accent, 0.9);
            }
            if ui.click && hov {
                open = Some(icon);
            }
        }
        ui.pop_clip();
        if let Some(icon) = open {
            self.open(&icon);
            self.left_tab = 0;
        }
    }

    fn draw_tree(&mut self, ui: &mut Ui<'_>, area: [f32; 4]) {
        let (ink, accent) = (ui.ink, ui.accent);
        let rows = self.rows_of();
        self.rows.clear();
        // the list, then the plate at its bottom, then the buttons
        let tools_h = 84.0;
        let list = [area[0], area[1], area[2], area[3] - tools_h];
        let content = (rows.len() as f32 + 1.0) * ROW_H + 12.0;
        let max_scroll = (content - list[3]).max(0.0);
        if ui.scroll != 0.0 && inside(ui.mouse, list) {
            self.tree_scroll += ui.scroll;
        }
        self.tree_scroll = self.tree_scroll.clamp(0.0, max_scroll);
        let scroll = ui.ease(9_004, self.tree_scroll, 18.0);
        ui.push_clip(list);
        // (id, eye pressed, fold pressed)
        let mut click_row: Option<(u32, bool, bool)> = None;
        // while a layer is dragged: the list scrolls when it is near an edge, and the place it would land is found as the rows go by
        let dragging = self.drag.as_ref().filter(|d| d.active).map(|d| d.id);
        let mut hint: Option<(Drop, [f32; 4])> = None;
        let mut last_bottom = list[1];
        if dragging.is_some() && inside(ui.mouse, [list[0], list[1] - 30.0, list[2], list[3] + 60.0]) {
            if ui.mouse[1] < list[1] + 26.0 {
                self.tree_scroll = (self.tree_scroll - 8.0).max(0.0);
            } else if ui.mouse[1] > list[1] + list[3] - 26.0 {
                self.tree_scroll = (self.tree_scroll + 8.0).min(max_scroll);
            }
        }
        let mut y = list[1] + 6.0 - scroll;
        for row in &rows {
            let r = [list[0] + 6.0, y, list[2] - 12.0, ROW_H - 2.0];
            y += ROW_H;
            if r[1] + r[3] < list[1] || r[1] > list[1] + list[3] {
                continue;
            }
            self.rows.push((row.id, r));
            last_bottom = r[1] + r[3];
            if let Some(dragged) = dragging {
                if inside(ui.mouse, [r[0], r[1] - 1.0, r[2], r[3] + 2.0]) && !self.doc.as_ref().is_some_and(|d| d.is_within(dragged, row.id)) {
                    let t = (ui.mouse[1] - r[1]) / r[3];
                    hint = Some(if row.group {
                        if t < 0.3 { (Drop::InFront(row.id), [r[0], r[1] - 1.0, r[2], 2.0]) } else { (Drop::Into(row.id), r) }
                    } else if t < 0.3 {
                        (Drop::InFront(row.id), [r[0] + row.depth as f32 * 16.0, r[1] - 1.0, r[2] - row.depth as f32 * 16.0, 2.0])
                    } else if t > 0.7 {
                        (Drop::Behind(row.id), [r[0] + row.depth as f32 * 16.0, r[1] + r[3] - 1.0, r[2] - row.depth as f32 * 16.0, 2.0])
                    } else {
                        (Drop::Pair(row.id), r)
                    });
                }
            }
            let on = self.sel == Some(Sel::Node(row.id)) || self.also.contains(&row.id);
            let hov = ui.hit(r) && inside(ui.mouse, list);
            if on {
                ui.fill(r, 8.0, accent, 0.22);
            } else if hov {
                ui.fill(r, 8.0, ink, 0.06);
            }
            let x0 = r[0] + 6.0 + row.depth as f32 * 16.0;
            let dim = if row.shown { 1.0 } else { 0.4 };
            let chev = [x0 - 2.0, r[1], 18.0, r[3]];
            if row.group {
                ui.glyph(x0 + 7.0, r[1] + r[3] / 2.0, 13.0, if row.open { "chevron-down" } else { "chevron-right" }, ink, 0.7);
            }
            let tx = x0 + 20.0;
            if row.group {
                ui.glyph(tx + 11.0, r[1] + r[3] / 2.0, 16.0, "folder-open", ink, 0.6 * dim);
            } else if let Some(pic) = self.thumb(row.id) {
                ui.fill([tx, r[1] + 2.0, 24.0, 24.0], 5.0, [0.62, 0.64, 0.68], 0.45 * dim);
                ui.picture([tx + 1.0, r[1] + 3.0, 22.0, 22.0], &pic, [0.0, 0.0, 48.0, 48.0], dim);
            }
            let label = fit(ui, &row.name, 12.5, false, r[2] - (tx - r[0]) - 62.0);
            ui.text_raw(tx + 32.0, r[1] + 19.0, &label, 12.5, false, ink, 0.92 * dim, Align::Left);
            let eye = [r[0] + r[2] - 30.0, r[1], 26.0, r[3]];
            let eye_hov = ui.hit(eye) && inside(ui.mouse, list);
            ui.glyph(eye[0] + 13.0, eye[1] + eye[3] / 2.0, 14.0, if row.shown { "eye" } else { "eye-off" }, ink, if eye_hov { 0.95 } else if row.shown { 0.5 } else { 0.8 });
            if ui.click && hov {
                if eye_hov {
                    click_row = Some((row.id, true, false));
                } else if row.group && ui.hit(chev) {
                    click_row = Some((row.id, false, true));
                } else {
                    click_row = Some((row.id, false, false));
                    // a press on a row may become a drag
                    if !ui.mods.shift && !ui.mods.ctrl {
                        self.drag = Some(Drag { id: row.id, from: ui.mouse, active: false });
                    }
                }
            }
        }
        // the plate
        let r = [list[0] + 6.0, y + 4.0, list[2] - 12.0, ROW_H - 2.0];
        if r[1] + r[3] >= list[1] && r[1] <= list[1] + list[3] {
            let on = self.sel == Some(Sel::Background);
            let hov = ui.hit(r) && inside(ui.mouse, list);
            if on {
                ui.fill(r, 8.0, accent, 0.22);
            } else if hov {
                ui.fill(r, 8.0, ink, 0.06);
            }
            ui.glyph(r[0] + 18.0, r[1] + r[3] / 2.0, 15.0, "layers", ink, 0.7);
            ui.text(r[0] + 40.0, r[1] + 19.0, "Background", 12.5, false, ink, 0.92, Align::Left);
            if ui.click && hov {
                self.sel = Some(Sel::Background);
                self.also.clear();
            }
        }
        // dropped below the last row: behind everything
        if let (Some(dragged), None) = (dragging, &hint) {
            let first = self.doc.as_ref().and_then(|d| d.nodes.first()).map(|n| n.id);
            if let Some(f) = first {
                if ui.mouse[1] > last_bottom && inside(ui.mouse, list) && f != dragged {
                    hint = Some((Drop::Behind(f), [list[0] + 6.0, last_bottom + 1.0, list[2] - 12.0, 2.0]));
                }
            }
        }
        if let Some((_, mark)) = &hint {
            let accent = ui.accent;
            if mark[3] <= 2.5 {
                ui.fill([mark[0], mark[1], mark[2], 3.0], 1.5, accent, 1.0);
            } else {
                ui.fill([mark[0] - 1.0, mark[1] - 1.0, mark[2] + 2.0, mark[3] + 2.0], 9.0, accent, 0.9);
                ui.fill(*mark, 8.0, card_color(ui.dark), 0.9);
                ui.fill(*mark, 8.0, accent, 0.25);
            }
        }
        ui.pop_clip();

        // the dragged layer follows the pointer
        if let Some(d) = self.drag.as_mut() {
            if !d.active && ui.held && (ui.mouse[0] - d.from[0]).hypot(ui.mouse[1] - d.from[1]) > 6.0 {
                d.active = true;
            }
        }
        if let Some(dragged) = dragging {
            let name = self.doc.as_ref().and_then(|d| d.find(dragged)).map(|n| n.name.clone()).unwrap_or_default();
            let g = [area[0] + 10.0, ui.mouse[1] - ROW_H / 2.0, area[2] - 20.0, ROW_H - 2.0];
            ui.fill([g[0] - 1.0, g[1] - 1.0, g[2] + 2.0, g[3] + 2.0], 9.0, accent, 0.8);
            ui.fill(g, 8.0, card_color(ui.dark), 0.96);
            ui.text_raw(g[0] + 14.0, g[1] + 19.0, &name, 12.5, true, ink, 0.95, Align::Left);
            ui.want_cursor(Cursor::Pointer);
        }
        if self.drag.is_some() && !ui.held {
            let d = self.drag.take().unwrap();
            if d.active {
                if let Some((to, _)) = hint {
                    let moved = self.doc.as_mut().is_some_and(|doc| doc.move_node(d.id, to));
                    if moved {
                        self.also.clear();
                        self.sel = Some(Sel::Node(d.id));
                        self.touch();
                    }
                }
            }
        }

        if let Some((id, eye, fold)) = click_row {
            if eye {
                if let Some(mut p) = self.props_of(id) {
                    p.visible = !p.visible;
                    self.set_props(id, p);
                }
            } else if fold {
                if !self.collapsed.remove(&id) {
                    self.collapsed.insert(id);
                }
            } else {
                let order: Vec<u32> = rows.iter().map(|r| r.id).collect();
                self.pick(id, ui.mods, &order);
            }
        }

        // what can be done with the selected layer
        let tools = [area[0] + 10.0, area[1] + area[3] - tools_h + 10.0, area[2] - 20.0, tools_h - 20.0];
        ui.fill([area[0], tools[1] - 10.0, area[2], 1.0], 0.0, ink, 0.09);
        let picked = self.picked();
        let sel = picked.first().copied();
        let is_group = picked.len() == 1 && sel.and_then(|id| self.doc.as_ref()?.find(id)).is_some_and(|n| matches!(n.kind, Kind::Group(_)));
        let bw = (tools[2] - 8.0) / 3.0;
        let b = |i: usize| [tools[0] + (i % 3) as f32 * (bw + 4.0), tools[1] + (i / 3) as f32 * 34.0, bw, 28.0];
        let mut action = None;
        for (i, (glyph, label, a)) in [("arrow-up", "Forward", 0), ("arrow-down", "Backward", 1), ("group", "Group", 2), ("ungroup", "Ungroup", 3), ("trash-2", "Delete", 4), ("copy", "Duplicate", 5)].iter().enumerate() {
            let usable = sel.is_some() && (*a != 3 || is_group);
            // the name of a tool is shown above them while the pointer is on it
            if ui.hit(b(i)) {
                ui.text(tools[0] + tools[2] / 2.0, tools[1] - 16.0, label, 11.5, true, ink, 0.8, Align::Center);
            }
            if ui.icon_button(b(i), glyph, "", false) && usable {
                action = Some(*a);
            }
        }
        if let Some(a) = action {
            self.layer_action(&picked, a);
        }
    }

    fn layer_action(&mut self, ids: &[u32], action: usize) {
        let mut new_sel = Vec::new();
        let mut deselect = false;
        let Some(d) = self.doc.as_mut() else { return };
        let mut ok = false;
        match action {
            // each one steps, the front ones first when going forward and the back ones first when going back
            0 | 1 => {
                let up = action == 0;
                let mut order = Vec::new();
                front_to_back(&d.nodes, &mut order);
                order.retain(|id| ids.contains(id));
                if !up {
                    order.reverse();
                }
                for id in order {
                    ok |= d.shift(id, up);
                }
            }
            2 => {
                // the main one is grouped, and the others go into that group
                if let Some(&first) = ids.first() {
                    if let Some(g) = d.group(first) {
                        for &id in &ids[1..] {
                            d.move_node(id, Drop::Into(g));
                        }
                        new_sel.push(g);
                        ok = true;
                    }
                }
            }
            3 => {
                for &id in ids {
                    ok |= d.ungroup(id);
                }
            }
            4 => {
                for &id in ids {
                    ok |= d.remove(id).is_some();
                }
                deselect = ok;
            }
            5 => {
                for &id in ids {
                    if let Some(copy) = d.duplicate(id) {
                        new_sel.push(copy);
                        ok = true;
                    }
                }
            }
            _ => {}
        }
        if !new_sel.is_empty() {
            self.sel = Some(Sel::Node(new_sel[0]));
            self.also = new_sel[1..].to_vec();
        }
        if deselect {
            self.sel = None;
            self.also.clear();
        }
        if ok {
            self.prune_picked();
            self.touch();
        }
    }

    // ------------------------------------------------------------------------------------------------ more to do with an icon

    /// Flip or turn the drawing of the picked layers (all the layers of a picked group).
    fn svg_op(&mut self, op: SvgOp) {
        let ids = self.picked();
        let Some(d) = self.doc.as_mut() else { return };
        fn layers(node: &icondoc::Node, out: &mut Vec<u32>) {
            match &node.kind {
                Kind::Layer(_) => out.push(node.id),
                Kind::Group(g) => g.children.iter().for_each(|c| layers(c, out)),
            }
        }
        let mut all = Vec::new();
        for id in ids {
            if let Some(n) = d.find(id) {
                layers(n, &mut all);
            }
        }
        let mut changed = false;
        for id in all {
            if let Some(Kind::Layer(l)) = d.find_mut(id).map(|n| &mut n.kind) {
                if let Some(t) = iconrender::transform_svg(&l.svg, op) {
                    l.svg = t;
                    changed = true;
                }
            }
        }
        if changed {
            self.touch();
        }
    }

    /// Put the picked layers back in the middle at their own size.
    fn centre_picked(&mut self) {
        for id in self.picked() {
            if let Some(mut p) = self.props_of(id) {
                p.offset = [0.0, 0.0];
                p.scale = 1.0;
                self.set_props(id, p);
            }
        }
    }

    /// The icon as it was made: a new document from the SVG of the icon it replaces.
    fn original_of(&self, name: &str) -> Result<IconDoc, String> {
        match self.index.find(name, 128) {
            Some(p) if p.extension().is_some_and(|e| e == "svg") => std::fs::read_to_string(&p).map_err(|e| e.to_string()).and_then(|t| import_svg(name, &t)),
            Some(_) => Err("This icon is a PNG, so there is nothing to go back to.".to_string()),
            None => Err(format!("No icon called {name} was found.")),
        }
    }

    /// Throw away every change of this icon: it goes back to the original drawing. Undo brings the changes back.
    fn reset_current(&mut self) {
        let Some(name) = self.doc.as_ref().map(|d| d.name.clone()) else { return };
        match self.original_of(&name) {
            Ok(fresh) => {
                if let Some(old) = self.doc.take() {
                    self.undo.push(old);
                    self.redo.clear();
                }
                self.doc = Some(fresh);
                self.sel = None;
                self.also.clear();
                self.touch();
                self.note = format!("{name} is the original again.");
            }
            Err(e) => self.note = e,
        }
    }

    /// Remove every document and the theme made from them: all icons are the originals again.
    fn reset_all(&mut self) {
        let dir = icondoc::docs_dir();
        let mut n = 0;
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                if e.path().extension().is_some_and(|x| x == "memicon") && std::fs::remove_file(e.path()).is_ok() {
                    n += 1;
                }
            }
        }
        for d in iconrender::theme_dirs() {
            let _ = std::fs::remove_dir_all(d);
        }
        iconrender::touch_marker();
        self.save_due = false;
        self.undo.clear();
        self.redo.clear();
        self.sel = None;
        self.also.clear();
        let current = self.doc.as_ref().map(|d| d.name.clone());
        self.doc = current.and_then(|name| self.original_of(&name).ok());
        self.touch();
        self.save_due = false;
        self.confirm_reset_all = false;
        self.note = format!("{n} icons are the originals again.");
    }

    /// Write the icon in all six looks as large PNGs, to `~/Pictures/Memrium-Icons/<name>`.
    fn export_png(&mut self) {
        let Some(doc) = self.doc.clone() else { return };
        let home = std::env::var("HOME").unwrap_or_default();
        let dir = std::path::PathBuf::from(home).join("Pictures/Memrium-Icons").join(&doc.name);
        let (tune, flat) = (self.tune, self.flat);
        self.note = format!("Exporting {} to {}", doc.name, dir.display());
        let out = dir.clone();
        std::thread::spawn(move || {
            let _ = std::fs::create_dir_all(&out);
            let mut r = Renderer::new();
            r.tune = tune;
            r.flat = flat;
            for look in GRID_ORDER {
                let img = r.render(&doc, look, 1024);
                let _ = img.save(out.join(format!("{}.png", look.label().to_lowercase().replace(' ', "-"))));
            }
        });
    }

    /// Write this icon (or, after the glass was tuned, all of them) into the icon theme, so that the shell shows them now.
    fn live_write(&mut self, all: bool) {
        if !self.live || self.live_busy.swap(true, Ordering::AcqRel) {
            self.live_pending |= self.live;
            return;
        }
        self.live_pending = false;
        let docs: Vec<IconDoc> = if all { icondoc::saved_docs() } else { self.doc.iter().cloned().collect() };
        let (tune, busy) = (self.tune, self.live_busy.clone());
        std::thread::spawn(move || {
            let last = docs.len().saturating_sub(1);
            for (i, d) in docs.iter().enumerate() {
                let _ = iconrender::build_one(d, tune, SOURCE_THEME, i == last);
            }
            busy.store(false, Ordering::Release);
        });
    }

    // ------------------------------------------------------------------------------------------------ the middle

    fn draw_stage(&mut self, ui: &mut Ui<'_>, area: [f32; 4]) {
        let stage = [area[0] + 16.0, area[1] + 16.0, area[2] - 32.0, area[3] - 32.0];
        match self.backdrop {
            0 => ui.grad(stage, 18.0, [0.0, 0.0, 0.0], [0.045, 0.045, 0.06], 1.0),
            1 => ui.grad(stage, 18.0, [0.97, 0.97, 0.985], [0.84, 0.86, 0.91], 1.0),
            2 => ui.grad(stage, 18.0, [0.95, 0.45, 0.35], [0.30, 0.25, 0.65], 1.0),
            // the photo lies under the whole window already
            _ if self.photo() => {}
            _ => ui.grad(stage, 18.0, [0.0, 0.0, 0.0], [0.045, 0.045, 0.06], 1.0),
        }
        let light = self.backdrop == 1;
        let sc = if light { [0.1, 0.1, 0.12] } else { [1.0, 1.0, 1.0] };

        // the looks (the system's styles, each light or dark), history and the backdrop
        let (mut style, dark) = self.look.parts();
        let mut mode = if dark { 1 } else { 0 };
        let mut changed = ui.segmented([stage[0] + 14.0, stage[1] + 14.0, 204.0, 28.0], &["Default", "Clear", "Tinted"], &mut style);
        changed |= ui.segmented([stage[0] + 226.0, stage[1] + 14.0, 116.0, 28.0], &["Light", "Dark"], &mut mode);
        if changed {
            self.look = Appearance::from_parts(style, mode == 1);
            self.outline = None;
        }
        let mut x = stage[0] + 350.0;
        for (glyph, undo) in [("undo-2", true), ("redo-2", false)] {
            let on = if undo { !self.undo.is_empty() } else { !self.redo.is_empty() };
            let r = [x, stage[1] + 14.0, 30.0, 28.0];
            let hov = on && ui.hit(r);
            ui.fill(r, 8.0, sc, if hov { 0.22 } else { 0.1 });
            ui.glyph(r[0] + 15.0, r[1] + 14.0, 15.0, glyph, sc, if on { 0.9 } else { 0.3 });
            if hov {
                ui.want_cursor(Cursor::Pointer);
                if ui.click {
                    if undo {
                        self.undo_step();
                    } else {
                        self.redo_step();
                    }
                }
            }
            x += 36.0;
        }
        // where the light comes from
        {
            let r = [x + 4.0, stage[1] + 14.0, 28.0, 28.0];
            let c = [r[0] + 14.0, r[1] + 14.0];
            ui.fill(r, 14.0, sc, 0.12);
            let a = self.tune.light.to_radians();
            ui.fill([c[0] + a.cos() * 8.5 - 3.5, c[1] + a.sin() * 8.5 - 3.5, 7.0, 7.0], 3.5, ui.accent, 1.0);
            if ui.hit(r) {
                ui.want_cursor(Cursor::Pointer);
                if ui.click {
                    self.dial_drag = true;
                }
            }
            if self.dial_drag {
                if ui.held {
                    let deg = (ui.mouse[1] - c[1]).atan2(ui.mouse[0] - c[0]).to_degrees().rem_euclid(360.0).round();
                    let t = GlassTune { light: deg, ..self.tune };
                    self.apply_tune(t);
                } else {
                    self.dial_drag = false;
                }
            }
            ui.text(r[0] + 38.0, r[1] + 19.0, &format!("{}°", self.tune.light.round()), 12.0, false, sc, 0.7, Align::Left);
        }
        // the ground of the stage
        let names = ["Dark", "Light", "Colour", "Photo"];
        {
            let r = [stage[0] + stage[2] - 92.0, stage[1] + 14.0, 78.0, 28.0];
            ui.fill(r, 14.0, sc, 0.12);
            ui.glyph(r[0] + 16.0, r[1] + 14.0, 15.0, "image", sc, 0.85);
            ui.text(r[0] + 30.0, r[1] + 19.0, names[self.backdrop], 12.0, false, sc, 0.85, Align::Left);
            if ui.hit(r) {
                ui.want_cursor(Cursor::Pointer);
                if ui.click {
                    self.backdrop = (self.backdrop + 1) % 4;
                    self.backdrop_key = None;
                }
            }
        }
        if self.backdrop == 3 && !self.walls.is_empty() {
            let name = fit(ui, &self.walls[self.wall].0, 12.0, false, 170.0);
            let step = {
                let mut step = 0i32;
                let y = stage[1] + 48.0;
                let right = stage[0] + stage[2] - 14.0;
                for (dx, glyph, d) in [(0.0, "›", 1), (-30.0, "‹", -1)] {
                    let r = [right - 26.0 + dx - if d == -1 { 170.0 } else { 0.0 }, y, 26.0, 24.0];
                    if ui.button(r, glyph, false) {
                        step = d;
                    }
                }
                ui.text(right - 26.0 - 85.0 - 2.0, y + 16.0, &name, 12.0, false, [1.0; 3], 0.9, Align::Center);
                step
            };
            if step != 0 {
                self.wall = (self.wall as i32 + step).rem_euclid(self.walls.len() as i32) as usize;
            }
        }

        // what is shown: the icon being edited, or all its looks side by side
        let mut view = self.view;
        if ui.segmented([stage[0] + stage[2] / 2.0 - 100.0, stage[1] + stage[3] - 42.0, 200.0, 28.0], &["Edit", "All looks"], &mut view) {
            self.view = view;
            self.cdrag = None;
        }
        let mut flat = usize::from(self.flat);
        if ui.segmented([stage[0] + 14.0, stage[1] + stage[3] - 42.0, 130.0, 28.0], &["Glass", "Flat"], &mut flat) {
            self.flat = flat == 1;
            // a new picture is wanted although the document is the same
            self.version += 1;
            self.thumbs.clear();
        }
        // the size of the icon
        {
            let y = stage[1] + stage[3] - 42.0;
            let r = [stage[0] + stage[2] - 150.0, y, 136.0, 28.0];
            ui.fill(r, 14.0, sc, 0.1);
            let mut z = self.zoom;
            for (label, dx, step) in [("−", 0.0, -0.1), ("+", 108.0, 0.1)] {
                let b = [r[0] + dx, r[1], 28.0, 28.0];
                if ui.hit(b) {
                    ui.want_cursor(Cursor::Pointer);
                    if ui.click {
                        z += step;
                    }
                }
                ui.text(b[0] + 14.0, b[1] + 19.0, label, 16.0, true, sc, 0.9, Align::Center);
            }
            let mid = [r[0] + 28.0, r[1], 80.0, 28.0];
            if ui.hit(mid) && ui.click {
                z = 1.0;
            }
            ui.text(mid[0] + 40.0, mid[1] + 18.5, &format!("{}%", (self.zoom * 100.0).round()), 12.5, false, sc, 0.85, Align::Center);
            if ui.scroll != 0.0 && ui.mods.ctrl && inside(ui.mouse, stage) {
                z += if ui.scroll > 0.0 { -0.1 } else { 0.1 };
            }
            self.set_zoom(z);
        }
        if self.view == 1 {
            self.draw_grid(ui, stage, sc);
            return;
        }

        // the icon
        let big = self.n() as f32;
        let (bx, by) = (stage[0] + stage[2] / 2.0 - big / 2.0, stage[1] + if self.backdrop == 3 { 78.0 } else { 58.0 });
        self.big_rect = [bx, by, big, big];
        let rect = self.big_rect;
        self.update_backdrop(rect);
        if let Some(s) = &self.shown {
            ui.picture(rect, &s.big, [0.0, 0.0, big, big], 1.0);
        }
        self.draw_outline(ui, rect);
        if let Some(sh) = &self.shown {
            // and smaller, as it is used
            // (the row of controls along the bottom of the stage keeps its place)
            let room = stage[1] + stage[3] - (by + big) - 30.0 - 50.0;
            let sizes: Vec<&(u32, Arc<Pic>)> = sh.small.iter().filter(|(n, _)| *n as f32 <= room.max(40.0)).collect();
            if !sizes.is_empty() {
                let total: f32 = sizes.iter().map(|(n, _)| *n as f32 + 28.0).sum::<f32>() - 28.0;
                let mut sx = stage[0] + stage[2] / 2.0 - total / 2.0;
                let row_h = sizes[0].0 as f32;
                for (n, pic) in sizes {
                    let f = *n as f32;
                    ui.picture([sx, by + big + 20.0 + (row_h - f) / 2.0, f, f], pic, [0.0, 0.0, f, f], 1.0);
                    sx += f + 28.0;
                }
            }
        } else if self.doc.is_none() {
            ui.text(stage[0] + stage[2] / 2.0, by + big / 2.0, "Pick an icon in the Icons tab", 13.0, false, sc, 0.6, Align::Center);
        }
        self.eyedropper(ui);
    }

    /// The part of the wallpaper (in its pixels) that fills `stage`.
    fn wall_src(pic: &Pic, aspect: f32, stage: [f32; 4]) -> [f32; 4] {
        let want = stage[2] / stage[3];
        let (iw, ih) = (pic.w as f32, pic.h as f32);
        if aspect > want {
            let w = ih * want;
            [(iw - w) / 2.0, 0.0, w, ih]
        } else {
            let h = iw / want;
            [0.0, (ih - h) / 2.0, iw, h]
        }
    }

    /// What the photo shows at `rect` of the window, as a picture of `n` pixels: the glass is drawn over it.
    fn crop_behind(&self, rect: [f32; 4], n: u32) -> Option<Arc<image::RgbaImage>> {
        let (Some((_, pic, aspect)), Some(img)) = (&self.wall_pic, &self.wall_img) else { return None };
        let win = self.win;
        let src = Self::wall_src(pic, *aspect, win);
        let k = src[2] / win[2];
        let crop = [
            (src[0] + (rect[0] - win[0]) * k).round().max(0.0) as u32,
            (src[1] + (rect[1] - win[1]) * k).round().max(0.0) as u32,
            (rect[2] * k).round().max(1.0) as u32,
            (rect[3] * k).round().max(1.0) as u32,
        ];
        let (w, h) = (crop[2].min(img.width().saturating_sub(crop[0])), crop[3].min(img.height().saturating_sub(crop[1])));
        if w == 0 || h == 0 {
            return None;
        }
        let part = image::imageops::crop_imm(&**img, crop[0], crop[1], w, h).to_image();
        let mut part = image::imageops::resize(&part, n, n, image::imageops::FilterType::Triangle);
        for p in part.pixels_mut() {
            p[3] = 255;
        }
        Some(Arc::new(part))
    }

    fn rect_key(r: [f32; 4]) -> [i32; 4] {
        [r[0] as i32, r[1] as i32, r[2] as i32, r[3] as i32]
    }

    /// What is under the icon on the stage. Without the photo (or when the icon is not on the stage) there is none and the
    /// glass has nothing to show.
    fn update_backdrop(&mut self, rect: [f32; 4]) {
        let key = (self.backdrop_img_wall(), Self::rect_key(self.win), Self::rect_key(rect));
        if !self.photo() || self.view != 0 {
            if self.behind.take().is_some() {
                self.version += 1;
            }
            self.backdrop_key = None;
            return;
        }
        if self.backdrop_key == Some(key) {
            return;
        }
        self.backdrop_key = Some(key);
        if let Some(part) = self.crop_behind(rect, self.n()) {
            self.behind = Some(part);
            self.version += 1;
        }
    }

    /// What is under each of the six looks of the overview.
    fn update_grid_behind(&mut self, cells: &[[f32; 4]]) {
        let key = (self.backdrop_img_wall(), Self::rect_key(self.win), Self::rect_key(cells[0]));
        if !self.photo() {
            if self.grid_behind.take().is_some() {
                self.version += 1;
            }
            self.grid_key = None;
            return;
        }
        if self.grid_key == Some(key) {
            return;
        }
        self.grid_key = Some(key);
        self.grid_behind = Some(cells.iter().filter_map(|c| self.crop_behind(*c, GRID_N)).collect());
        self.version += 1;
    }

    fn backdrop_img_wall(&self) -> usize {
        self.wall_pic.as_ref().map_or(usize::MAX, |w| w.0)
    }

    /// The wallpaper picked, loaded (with its blurred copy for the panels) when first wanted and when another is picked.
    fn ensure_wall(&mut self) {
        if self.walls.is_empty() {
            self.walls = wallpapers::scan();
            // the one the icons look best on comes first, and is the one shown
            if let Some(p) = wallpapers::named(PREFERRED_WALLPAPER) {
                self.walls.retain(|w| w.1 != p.1);
                self.walls.insert(0, p);
                self.wall = 0;
            }
        }
        if self.walls.is_empty() {
            return;
        }
        self.wall = self.wall.min(self.walls.len() - 1);
        if self.wall_pic.as_ref().map(|w| w.0) != Some(self.wall) {
            let img = wallpapers::load(&self.walls[self.wall].1, 1600);
            self.wall_pic = img.as_ref().map(|i| (self.wall, Arc::new(Pic::from_rgba(i.width(), i.height(), i.as_raw())), i.width() as f32 / i.height() as f32));
            self.wall_blur = img.as_ref().map(|i| {
                // a few big soft pixels, scaled back up smoothly
                let (w, h) = (i.width(), i.height());
                let small = image::imageops::resize(i, 40, (40 * h / w).max(1), image::imageops::FilterType::Triangle);
                let soft = image::imageops::resize(&small, 480, (480 * h / w).max(1), image::imageops::FilterType::CatmullRom);
                (self.wall, Arc::new(Pic::from_rgba(soft.width(), soft.height(), soft.as_raw())))
            });
            self.wall_img = img.map(Arc::new);
            self.backdrop_key = None;
            self.grid_key = None;
        }
    }

    /// The photo is the background of the whole window.
    fn photo(&self) -> bool {
        self.backdrop == 3 && self.wall_pic.is_some()
    }

    /// The wallpaper over the whole window, or its blurred copy within `clip` (what a panel of frosted glass shows).
    fn paint_wall(&self, ui: &mut Ui<'_>, blurred: bool, clip: Option<[f32; 4]>) {
        let win = self.win;
        let Some((_, pic, aspect)) = &self.wall_pic else { return };
        let (pic, aspect) = match (blurred, &self.wall_blur) {
            (true, Some((_, b))) => (b, *aspect),
            _ => (pic, *aspect),
        };
        let src = Self::wall_src(pic, aspect, win);
        if let Some(c) = clip {
            ui.push_clip(c);
        }
        ui.picture(win, pic, src, 1.0);
        if clip.is_some() {
            ui.pop_clip();
        }
    }

    /// The ground of a side panel or the bar: frosted glass over the photo, or plain when there is none.
    fn panel_bg(&self, ui: &mut Ui<'_>, area: [f32; 4]) {
        let ink = ui.ink;
        if self.photo() {
            self.paint_wall(ui, true, Some(area));
            ui.fill(area, 0.0, card_color(ui.dark), if ui.dark { 0.62 } else { 0.55 });
            ui.fill([area[0], area[1], area[2], 1.0], 0.0, [1.0; 3], 0.10);
        } else {
            ui.fill(area, 0.0, card_color(ui.dark), 1.0);
        }
        let _ = ink;
    }

    /// The six looks of the icon side by side, drawn again as it changes. A click on one makes it the look being edited.
    fn draw_grid(&mut self, ui: &mut Ui<'_>, stage: [f32; 4], sc: [f32; 3]) {
        self.grid_rects.clear();
        let accent = ui.accent;
        let avail_h = stage[3] - 56.0 - 52.0;
        let cell = (GRID_N as f32).min(avail_h / 2.0 - 36.0).max(80.0);
        let gap_x = ((stage[2] - 3.0 * cell) / 4.0).max(8.0);
        let top = stage[1] + 56.0 + (avail_h - 2.0 * (cell + 36.0)).max(0.0) / 2.0;
        let cells: Vec<[f32; 4]> = (0..6).map(|k| [stage[0] + gap_x + (k % 3) as f32 * (cell + gap_x), top + (k / 3) as f32 * (cell + 36.0), cell, cell]).collect();
        self.update_grid_behind(&cells);
        let Some((_, pics)) = self.grid.clone() else {
            ui.text(stage[0] + stage[2] / 2.0, stage[1] + stage[3] / 2.0, "Drawing the looks…", 13.0, false, sc, 0.6, Align::Center);
            return;
        };
        let mut chosen = None;
        for (k, look) in GRID_ORDER.iter().enumerate() {
            let (col, row) = (k % 3, k / 3);
            let r = [stage[0] + gap_x + col as f32 * (cell + gap_x), top + row as f32 * (cell + 36.0), cell, cell];
            self.grid_rects.push((*look, r));
            let hov = ui.hit([r[0] - 8.0, r[1] - 8.0, r[2] + 16.0, r[3] + 40.0]);
            if *look == self.look {
                ui.fill([r[0] - 8.0, r[1] - 8.0, r[2] + 16.0, r[3] + 40.0], 14.0, accent, 0.22);
            } else if hov {
                ui.fill([r[0] - 8.0, r[1] - 8.0, r[2] + 16.0, r[3] + 40.0], 14.0, sc, 0.07);
            }
            if let Some((_, pic)) = pics.iter().find(|(l, _)| l == look) {
                ui.picture(r, pic, [0.0, 0.0, GRID_N as f32, GRID_N as f32], 1.0);
            }
            ui.text(r[0] + r[2] / 2.0, r[1] + r[3] + 22.0, look.label(), 13.0, true, sc, 0.9, Align::Center);
            if hov {
                ui.want_cursor(Cursor::Pointer);
                if ui.click {
                    chosen = Some(*look);
                }
            }
        }
        if let Some(l) = chosen {
            self.look = l;
            self.view = 0;
            self.outline = None;
        }
    }

    /// The selected layer or group is outlined on the icon, with handles at the corners of its box. A press picks the layer
    /// under the pointer, and dragging moves it; dragging a corner handle scales it. The arrow keys nudge it.
    fn draw_outline(&mut self, ui: &mut Ui<'_>, rect: [f32; 4]) {
        let eyedrop = self.picker.as_ref().is_some_and(|p| p.eyedrop);
        let Some(doc) = self.doc.clone() else { return };
        let accent = ui.accent;

        // moving or scaling what is being dragged
        let fit = 1.0 - 2.0 * doc.margin.clamp(0.0, 0.3);
        if let Some(cd) = &self.cdrag {
            if ui.held {
                let (id, scale, from, start) = (cd.id, cd.scale, cd.from, cd.props);
                let others = cd.others.clone();
                let mut p = start;
                if scale {
                    let c = [rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0];
                    let d0 = (from[0] - c[0]).hypot(from[1] - c[1]).max(8.0);
                    let d1 = (ui.mouse[0] - c[0]).hypot(ui.mouse[1] - c[1]);
                    p.scale = (start.scale * d1 / d0).clamp(0.1, 4.0);
                } else {
                    p.offset = [start.offset[0] + (ui.mouse[0] - from[0]) / (rect[2] * fit), start.offset[1] + (ui.mouse[1] - from[1]) / (rect[3] * fit)];
                }
                self.set_props(id, p);
                for (oid, op) in others {
                    let mut q = op;
                    if scale {
                        q.scale = (op.scale * p.scale / start.scale.max(0.01)).clamp(0.1, 4.0);
                    } else {
                        q.offset = [op.offset[0] + p.offset[0] - start.offset[0], op.offset[1] + p.offset[1] - start.offset[1]];
                    }
                    self.set_props(oid, q);
                }
                ui.want_cursor(if scale { Cursor::ResizeNwse } else { Cursor::Pointer });
            } else {
                self.cdrag = None;
            }
        }

        if let Some(Sel::Node(id)) = self.sel {
            let stale = self.outline.as_ref().map_or(true, |(i, v, l, _)| *i != id || *v != self.version || *l != self.look);
            if stale {
                let mask = self.drawer.silhouette(&doc, self.look, id, self.n());
                self.sel_box = mask.as_ref().and_then(|m| {
                    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
                    for (x, y, p) in m.enumerate_pixels() {
                        if p[3] > 40 {
                            x0 = x0.min(x);
                            y0 = y0.min(y);
                            x1 = x1.max(x);
                            y1 = y1.max(y);
                        }
                    }
                    (x0 <= x1).then(|| [x0 as f32, y0 as f32, (x1 - x0) as f32, (y1 - y0) as f32])
                });
                self.outline = mask.map(|m| (id, self.version, self.look, Arc::new(Pic::from_rgba(self.n(), self.n(), outline_of(&m, accent).as_raw()))));
            }
            if let Some((_, _, _, pic)) = &self.outline {
                ui.picture(rect, pic, [0.0, 0.0, rect[2], rect[3]], 1.0);
            }
        } else {
            self.sel_box = None;
        }
        // the box and its corner handles
        let mut handles: Vec<[f32; 2]> = Vec::new();
        if let (Some(Sel::Node(_)), Some(b)) = (self.sel, self.sel_box) {
            let r = [rect[0] + b[0], rect[1] + b[1], b[2], b[3]];
            for e in [[r[0], r[1], r[2], 1.0], [r[0], r[1] + r[3], r[2], 1.0], [r[0], r[1], 1.0, r[3]], [r[0] + r[2], r[1], 1.0, r[3] + 1.0]] {
                ui.fill(e, 0.0, accent, 0.55);
            }
            handles = vec![[r[0], r[1]], [r[0] + r[2], r[1]], [r[0], r[1] + r[3]], [r[0] + r[2], r[1] + r[3]]];
            for h in &handles {
                ui.fill([h[0] - 6.0, h[1] - 6.0, 12.0, 12.0], 4.0, accent, 1.0);
                ui.fill([h[0] - 4.5, h[1] - 4.5, 9.0, 9.0], 3.0, [1.0; 3], 1.0);
            }
        }

        if eyedrop || self.cdrag.is_some() {
            return;
        }
        let over_handle = handles.iter().any(|h| (ui.mouse[0] - h[0]).abs() < 9.0 && (ui.mouse[1] - h[1]).abs() < 9.0);
        if over_handle {
            ui.want_cursor(Cursor::ResizeNwse);
        } else if inside(ui.mouse, rect) {
            ui.want_cursor(Cursor::Pointer);
        }
        if ui.click && (inside(ui.mouse, rect) || over_handle) {
            if over_handle {
                if let Some(Sel::Node(id)) = self.sel {
                    if let Some(p) = self.props_of(id) {
                        let others = self.also.iter().filter_map(|&o| Some((o, self.props_of(o)?))).collect();
                        self.cdrag = Some(CanvasDrag { id, scale: true, from: ui.mouse, props: p, others });
                    }
                }
                return;
            }
            let (x, y) = (ui.mouse[0] - rect[0], ui.mouse[1] - rect[1]);
            match self.drawer.hit(&doc, self.look, self.n(), x, y) {
                Some(id) => {
                    // a layer that is already picked keeps the company; Shift or Ctrl adds or takes out
                    let in_company = self.picked().contains(&id);
                    if ui.mods.shift || ui.mods.ctrl {
                        self.pick(id, KeyMods { shift: false, ctrl: true, alt: false }, &[]);
                    } else if !in_company {
                        self.pick(id, KeyMods::default(), &[]);
                    } else if self.sel != Some(Sel::Node(id)) {
                        // the pressed layer becomes the main one
                        self.also.retain(|&x| x != id);
                        if let Some(Sel::Node(m)) = self.sel {
                            self.also.push(m);
                        }
                        self.sel = Some(Sel::Node(id));
                    }
                    if self.picked().contains(&id) {
                        if let Some(p) = self.props_of(id) {
                            let others = self.also.iter().filter_map(|&o| Some((o, self.props_of(o)?))).collect();
                            self.cdrag = Some(CanvasDrag { id, scale: false, from: ui.mouse, props: p, others });
                        }
                    }
                }
                None => {
                    self.sel = Some(Sel::Background);
                    self.also.clear();
                }
            }
        }
    }

    /// While the eyedropper is on, the colour under the pointer is shown beside it and a click takes it.
    fn eyedropper(&mut self, ui: &mut Ui<'_>) {
        let Some(pk) = &self.picker else { return };
        if !pk.eyedrop {
            return;
        }
        ui.want_cursor(Cursor::Crosshair);
        let m = ui.mouse;
        let rect = self.big_rect;
        let found = self.shown.as_ref().and_then(|s| {
            if !inside(m, rect) {
                return None;
            }
            let x = (((m[0] - rect[0]) / rect[2]) * s.img.width() as f32) as u32;
            let y = (((m[1] - rect[1]) / rect[3]) * s.img.height() as f32) as u32;
            let p = s.img.get_pixel(x.min(s.img.width() - 1), y.min(s.img.height() - 1)).0;
            (p[3] > 24).then(|| [p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0])
        });
        if let Some(c) = found {
            let r = [m[0] + 14.0, m[1] - 40.0, 54.0, 28.0];
            ui.fill([r[0] - 2.0, r[1] - 2.0, r[2] + 4.0, r[3] + 4.0], 9.0, [1.0; 3], 0.95);
            ui.fill(r, 7.0, c, 1.0);
            ui.text_raw(r[0] + r[2] + 8.0, r[1] + 19.0, &to_hex(c), 12.0, true, [1.0; 3], 0.95, Align::Left);
            if ui.click {
                if let Some(pk) = &mut self.picker {
                    pk.hsv = rgb_to_hsv(c);
                    pk.hex = to_hex(c);
                    pk.eyedrop = false;
                    pk.fresh = true;
                }
                self.apply_picker();
            }
        } else if ui.click {
            // a click elsewhere gives the eyedropper up, and nothing else
            if let Some(pk) = &mut self.picker {
                pk.eyedrop = false;
                pk.fresh = true;
            }
        }
    }

    // ------------------------------------------------------------------------------------------------ the inspector

    fn draw_inspector(&mut self, ui: &mut Ui<'_>, area: [f32; 4]) {
        let (ink, accent) = (ui.ink, ui.accent);
        self.panel_bg(ui, area);
        ui.fill([area[0], area[1], 1.0, area[3]], 0.0, ink, 0.09);
        let x = area[0] + 20.0;
        let cw = area[2] - 40.0;
        let Some(doc) = self.doc.clone() else {
            ui.text(x, area[1] + 40.0, "No icon is open.", 13.0, false, ink, 0.6, Align::Left);
            return;
        };

        // which look is being edited
        let mut y = area[1] + 22.0;
        let title = match self.sel {
            Some(Sel::Background) => "Background".to_string(),
            Some(Sel::Node(id)) => doc.find(id).map(|n| n.name.clone()).unwrap_or_default(),
            None => doc.name.clone(),
        };
        let shown = fit(ui, &title, 15.0, true, cw - 120.0);
        ui.text_raw(x, y + 6.0, &shown, 15.0, true, ink, 0.95, Align::Left);
        ui.text(x + cw, y + 6.0, self.look.label(), 11.5, true, accent, 0.9, Align::Right);
        y += 34.0;
        let own = match self.sel {
            Some(Sel::Background) => doc.background.is_set(self.look),
            Some(Sel::Node(id)) => doc.find(id).map_or(true, |n| n.props().is_set(self.look)),
            None => true,
        };
        if self.look != Appearance::Default {
            let parent = match self.look {
                Appearance::ClearDark => "Clear (light)",
                Appearance::TintedDark => "Tinted (light)",
                _ => "Default",
            };
            let note = if own { format!("Changes some settings; the rest follows {parent}.") } else { format!("Follows {parent}. What you change here stays in this look.") };
            let note = fit(ui, &note, 11.5, false, cw - 100.0);
            ui.text_raw(x, y, &note, 11.5, false, ink, 0.55, Align::Left);
            if own && reset_link(ui, x + cw, y, "Inherit all") {
                let look = self.look;
                let sel = self.sel;
                if let Some(d) = self.doc.as_mut() {
                    match sel {
                        Some(Sel::Background) => d.background.clear(look),
                        Some(Sel::Node(id)) => {
                            if let Some(n) = d.find_mut(id) {
                                n.props_mut().clear(look);
                            }
                        }
                        None => {}
                    }
                }
                self.touch();
            }
            y += 20.0;
        }

        // the settings scroll under the header
        let list = [area[0] + 1.0, y, area[2] - 1.0, area[1] + area[3] - y];
        let max_scroll = (self.insp_height - list[3] + 12.0).max(0.0);
        if ui.scroll != 0.0 && inside(ui.mouse, list) {
            self.insp_scroll += ui.scroll;
        }
        self.insp_scroll = self.insp_scroll.clamp(0.0, max_scroll);
        let off = ui.ease(9_002, self.insp_scroll, 18.0);
        // what is scrolled out of the list takes no clicks
        let (mouse, click) = (ui.mouse, ui.click);
        if !inside(ui.mouse, list) {
            ui.mouse = [-1000.0; 2];
            ui.click = false;
        }
        ui.push_clip(list);
        let mut f = Form::new(x, list[1] + 14.0 - off, cw, 36.0);
        let start = f.y;
        let mut open: Option<(PickTarget, [f32; 3], f32)> = None;

        match self.sel {
            Some(Sel::Node(id)) => {
                if let Some(node) = doc.find(id) {
                    let mut name = node.name.clone();
                    if f.text(ui, "Name", "node-name", &mut name, None) {
                        if let Some(n) = self.doc.as_mut().and_then(|d| d.find_mut(id)) {
                            n.name = name;
                        }
                        self.touch();
                    }
                    let is_group = matches!(node.kind, Kind::Group(_));
                    let before = iconrender::effective_props(node, self.look);
                    let mut p = before;

                    section_icon(ui, x, f.y - 4.0, "palette", "Color");
                    f.skip(18.0);
                    if !is_group {
                        // the layer's own colours, one colour, or a gradient
                        let mut mode = if p.gradient.is_some() { 2 } else if p.fill.is_some() { 1 } else { 0 };
                        ui.row(f.x, f.y, "Paint");
                        if ui.segmented([f.x + 110.0, f.y - 15.0, f.cw - 110.0, 30.0], &["Original", "Solid", "Gradient"], &mut mode) {
                            match mode {
                                0 => {
                                    p.fill = None;
                                    p.gradient = None;
                                }
                                1 => {
                                    p.gradient = None;
                                    p.fill = Some(p.fill.unwrap_or([0.5; 3]));
                                }
                                _ => p.gradient = Some(Gradient { a: p.fill.unwrap_or(Gradient::default().a), ..Gradient::default() }),
                            }
                        }
                        f.skip(36.0);
                        match p.gradient {
                            Some(mut g) => {
                                if swatch(ui, &mut f, "From", Some(g.a)) {
                                    open = Some((PickTarget::GradFrom, g.a, f.y - 36.0));
                                }
                                if swatch(ui, &mut f, "To", Some(g.b)) {
                                    open = Some((PickTarget::GradTo, g.b, f.y - 36.0));
                                }
                                f.toggle(ui, "Radial", &mut g.radial);
                                if !g.radial {
                                    f.slider(ui, "Angle", &mut g.angle, 0.0, 360.0, "°");
                                }
                                p.gradient = Some(g);
                            }
                            None if p.fill.is_some() => {
                                if swatch(ui, &mut f, "Fill", p.fill) {
                                    open = Some((PickTarget::LayerFill, p.fill.unwrap_or([0.5; 3]), f.y - 36.0));
                                }
                            }
                            None => {}
                        }
                    }
                    percent(ui, &mut f, "Opacity", &mut p.opacity);
                    let step = f.stepper(ui, "Blend mode", p.blend.label());
                    if step != 0 {
                        let at = Blend::ALL.iter().position(|b| *b == p.blend).unwrap_or(0) as i32;
                        p.blend = Blend::ALL[(at + step).rem_euclid(Blend::ALL.len() as i32) as usize];
                    }

                    f.skip(8.0);
                    section_icon(ui, x, f.y - 4.0, "sparkles", "Liquid Glass");
                    f.skip(18.0);
                    if is_group {
                        pick(ui, &mut f, "Mode", &mut p.glass_mode, GlassMode::ALL, GlassMode::label);
                    }
                    if is_group && p.glass_mode == GlassMode::Individual {
                        f.note(ui, "Each layer in the group has its own glass.", true);
                    } else {
                        glass_rows(ui, &mut f, &mut p.glass);
                    }

                    f.skip(8.0);
                    section_icon(ui, x, f.y - 4.0, "sliders-horizontal", "Composition");
                    f.skip(18.0);
                    f.toggle(ui, "Visible", &mut p.visible);
                    f.slider(ui, "Move across", &mut p.offset[0], -0.5, 0.5, "");
                    f.slider(ui, "Move down", &mut p.offset[1], -0.5, 0.5, "");
                    f.slider(ui, "Scale", &mut p.scale, 0.2, 2.0, "×");
                    if p != before {
                        self.set_props(id, p);
                    }
                    // flip, turn and centre the drawing
                    let bw = (f.cw - 3.0 * 4.0) / 4.0;
                    let mut op = None;
                    let mut centre = false;
                    for (i, (glyph, o)) in [("flip-horizontal", Some(SvgOp::FlipH)), ("flip-vertical", Some(SvgOp::FlipV)), ("rotate-cw", Some(SvgOp::RotateCw)), ("crosshair", None)].iter().enumerate() {
                        if ui.icon_button([f.x + i as f32 * (bw + 4.0), f.y - 12.0, bw, 28.0], glyph, "", false) {
                            op = *o;
                            centre = o.is_none();
                        }
                    }
                    f.skip(40.0);
                    if let Some(o) = op {
                        self.svg_op(o);
                    }
                    if centre {
                        self.centre_picked();
                    }
                }
            }
            Some(Sel::Background) => {
                let before = iconrender::background_for(&doc, self.look);
                let mut b = before;
                section_icon(ui, x, f.y - 4.0, "square-round-corner", "Plate");
                f.skip(18.0);
                pick(ui, &mut f, "Shape", &mut b.shape, Shape::ALL, Shape::label);
                pick(ui, &mut f, "Fill", &mut b.fill, FillKind::ALL, FillKind::label);
                if b.fill != FillKind::None {
                    if swatch(ui, &mut f, if b.fill == FillKind::Solid { "Colour" } else { "Top" }, Some(b.top)) {
                        open = Some((PickTarget::BgTop, b.top, f.y - 36.0));
                    }
                    if b.fill == FillKind::Gradient && swatch(ui, &mut f, "Bottom", Some(b.bottom)) {
                        open = Some((PickTarget::BgBottom, b.bottom, f.y - 36.0));
                    }
                }
                f.skip(8.0);
                section_icon(ui, x, f.y - 4.0, "sparkles", "Liquid Glass");
                f.skip(18.0);
                glass_rows(ui, &mut f, &mut b.glass);
                if b != before {
                    self.set_background(b);
                }
                self.document_rows(ui, &mut f, x, doc.margin, doc.tint, &mut open);
            }
            None => {
                f.note(ui, "Click a layer on the icon or in the list.", false);
                f.note(ui, "Click the plate to change its shape and colour.", false);
                self.document_rows(ui, &mut f, x, doc.margin, doc.tint, &mut open);
            }
        }
        self.insp_height = f.y - start + 40.0;
        ui.pop_clip();
        ui.mouse = mouse;
        ui.click = click;
        if let Some((target, c, ay)) = open {
            self.open_picker(target, c, ay);
        }
    }

    /// Settings of the whole icon: the empty border, the colour of the tinted looks.
    fn document_rows(&mut self, ui: &mut Ui<'_>, f: &mut Form, x: f32, margin: f32, tint: [f32; 3], open: &mut Option<(PickTarget, [f32; 3], f32)>) {
        f.skip(8.0);
        section_icon(ui, x, f.y - 4.0, "image", "Icon");
        f.skip(18.0);
        let mut m = margin * 100.0;
        f.slider(ui, "Border", &mut m, 0.0, 20.0, "%");
        if (m / 100.0 - margin).abs() > 1e-4 {
            if let Some(d) = self.doc.as_mut() {
                d.margin = m / 100.0;
            }
            self.touch();
        }
        if swatch(ui, f, "Tint colour", Some(tint)) {
            *open = Some((PickTarget::TintColour, tint, f.y - 36.0));
        }
        f.toggle(ui, "Live in the shell", &mut self.live);
        let w = (f.cw - 8.0) / 2.0;
        let fx = f.x;
        let row = |i: usize, y: f32| [fx + i as f32 * (w + 8.0), y - 12.0, w, 28.0];
        if ui.icon_button(row(0, f.y), "file-image", "Export PNGs", false) {
            self.export_png();
        }
        if ui.icon_button(row(1, f.y), "rotate-ccw", "Back to original", false) {
            self.reset_current();
        }
        f.skip(38.0);
        let all = row(0, f.y);
        let all = [all[0], all[1], f.cw, all[3]];
        if ui.click && !ui.hit(all) {
            self.confirm_reset_all = false;
        }
        let label = if self.confirm_reset_all { "Press again: remove all changes of every icon" } else { "Reset all icons" };
        if ui.icon_button(all, "trash-2", label, self.confirm_reset_all) {
            if self.confirm_reset_all {
                self.reset_all();
            } else {
                self.confirm_reset_all = true;
            }
        }
        f.skip(38.0);
    }

    // ------------------------------------------------------------------------------------------------ the colour picker

    fn open_picker(&mut self, target: PickTarget, colour: [f32; 3], anchor_y: f32) {
        self.picker = Some(Picker { target, hsv: rgb_to_hsv(colour), hex: to_hex(colour), drag: 0, eyedrop: false, anchor_y, fresh: true, focus: false, selected: false, applied: colour });
    }

    fn picker_rect(&self, size: [f32; 2]) -> Option<[f32; 4]> {
        let pk = self.picker.as_ref()?;
        let (pw, ph) = (292.0, 326.0);
        let x = size[0] - RIGHT_W + 24.0;
        let y = (pk.anchor_y + 20.0).min(size[1] - BAR_H - ph - 8.0).max(8.0);
        Some([x, y, pw, ph])
    }

    /// Write the picker's colour into the document.
    fn apply_picker(&mut self) {
        let Some(pk) = self.picker.as_mut() else { return };
        let rgb = hsv_to_rgb(pk.hsv);
        if rgb.iter().zip(pk.applied).all(|(a, b)| (a - b).abs() < 1e-4) {
            return;
        }
        pk.applied = rgb;
        let target = pk.target;
        let look = self.look;
        match target {
            PickTarget::BgTop | PickTarget::BgBottom => {
                if let Some(mut b) = self.doc.as_ref().map(|d| iconrender::background_for(d, look)) {
                    if target == PickTarget::BgTop {
                        b.top = rgb;
                    } else {
                        b.bottom = rgb;
                    }
                    self.set_background(b);
                }
            }
            PickTarget::LayerFill => {
                if let Some(Sel::Node(id)) = self.sel {
                    if let Some(mut p) = self.props_of(id) {
                        p.fill = Some(rgb);
                        self.set_props(id, p);
                    }
                }
            }
            PickTarget::GradFrom | PickTarget::GradTo => {
                if let Some(Sel::Node(id)) = self.sel {
                    if let Some(mut p) = self.props_of(id) {
                        let mut g = p.gradient.unwrap_or_default();
                        if target == PickTarget::GradFrom {
                            g.a = rgb;
                        } else {
                            g.b = rgb;
                        }
                        p.gradient = Some(g);
                        self.set_props(id, p);
                    }
                }
            }
            PickTarget::TintColour => {
                let mut changed = false;
                if let Some(d) = self.doc.as_mut() {
                    if d.tint != rgb {
                        d.tint = rgb;
                        changed = true;
                    }
                }
                if changed {
                    self.touch();
                }
            }
        }
    }

    fn draw_picker(&mut self, ui: &mut Ui<'_>, panel: [f32; 4], host: &mut Host) {
        let ink = ui.ink;
        let dark = ui.dark;
        let Some(pk) = self.picker.as_mut() else { return };
        let fresh = std::mem::replace(&mut pk.fresh, false);
        // a click outside closes it (the eyedropper handles its own clicks)
        if ui.click && !fresh && !inside(ui.mouse, panel) && !pk.eyedrop {
            self.picker = None;
            return;
        }
        ui.fill([panel[0] - 3.0, panel[1] - 1.0, panel[2] + 6.0, panel[3] + 8.0], 20.0, [0.0; 3], 0.22);
        ui.fill(panel, 16.0, ink, 0.16);
        ui.fill([panel[0] + 1.0, panel[1] + 1.0, panel[2] - 2.0, panel[3] - 2.0], 15.0, if dark { [0.2, 0.2, 0.22] } else { [0.99, 0.99, 1.0] }, 1.0);
        let sv = [panel[0] + 16.0, panel[1] + 16.0, 260.0, 172.0];
        let hue = pk.hsv[0];
        // the square: saturation across, value down, drawn as strips
        let cols = 52;
        let cw = sv[2] / cols as f32;
        ui.push_clip(sv);
        for i in 0..cols {
            let s = (i as f32 + 0.5) / cols as f32;
            ui.grad([sv[0] + i as f32 * cw, sv[1], cw + 0.6, sv[3]], 0.0, hsv_to_rgb([hue, s, 1.0]), [0.0; 3], 1.0);
        }
        ui.pop_clip();
        if ui.click && inside(ui.mouse, sv) {
            pk.drag = 1;
        }
        let hue_r = [sv[0], sv[1] + sv[3] + 14.0, sv[2], 18.0];
        if ui.click && inside(ui.mouse, [hue_r[0] - 4.0, hue_r[1] - 6.0, hue_r[2] + 8.0, hue_r[3] + 12.0]) {
            pk.drag = 2;
        }
        if !ui.held {
            pk.drag = 0;
        }
        if pk.drag == 1 {
            pk.hsv[1] = ((ui.mouse[0] - sv[0]) / sv[2]).clamp(0.0, 1.0);
            pk.hsv[2] = 1.0 - ((ui.mouse[1] - sv[1]) / sv[3]).clamp(0.0, 1.0);
        }
        let mut h01 = hue / 360.0;
        ui.gradient_slider(hue_r, &mut h01, &|t| hsv_to_rgb([t * 360.0, 1.0, 1.0]));
        if pk.drag != 1 {
            pk.hsv[0] = h01 * 360.0;
        }
        let (hx, hy) = (sv[0] + pk.hsv[1] * sv[2], sv[1] + (1.0 - pk.hsv[2]) * sv[3]);
        ui.fill([hx - 8.0, hy - 8.0, 16.0, 16.0], 8.0, [1.0; 3], 1.0);
        ui.fill([hx - 6.0, hy - 6.0, 12.0, 12.0], 6.0, hsv_to_rgb(pk.hsv), 1.0);
        if ui.click && inside(ui.mouse, sv) || pk.drag == 1 {
            ui.want_cursor(Cursor::Crosshair);
        }

        // hex field (typing, Ctrl+C / Ctrl+V / Ctrl+A), swatch, copy and paste buttons, eyedropper
        let rgb = hsv_to_rgb(pk.hsv);
        let row_y = panel[1] + 256.0;
        if !pk.focus {
            pk.hex = to_hex(rgb);
        }
        let field = [sv[0], row_y, 104.0, 30.0];
        let hov = ui.hit(field);
        if hov {
            ui.want_cursor(Cursor::Text);
        }
        if ui.click {
            pk.focus = hov;
            pk.selected = hov;
        }
        if pk.focus {
            ui.fill([field[0] - 1.5, field[1] - 1.5, field[2] + 3.0, field[3] + 3.0], 8.0, ui.accent, 0.55);
        }
        ui.fill(field, 7.0, ink, if pk.focus || hov { 0.12 } else { 0.07 });
        let tw = ui.measure_raw(&pk.hex, 13.0, false);
        if pk.focus && pk.selected && !pk.hex.is_empty() {
            ui.fill([field[0] + 7.0, field[1] + 5.0, tw + 4.0, 20.0], 4.0, ui.accent, 0.4);
        }
        ui.text_raw(field[0] + 9.0, field[1] + 20.0, &pk.hex, 13.0, false, ink, 0.95, Align::Left);
        if pk.focus && !pk.selected {
            ui.fill([field[0] + 10.0 + tw, field[1] + 6.0, 1.5, 18.0], 0.0, ink, 0.9);
        }
        let mut x = sv[0] + 112.0;
        ui.fill([x, row_y, 30.0, 30.0], 8.0, ink, 0.25);
        ui.fill([x + 1.0, row_y + 1.0, 28.0, 28.0], 7.0, rgb, 1.0);
        x += 38.0;
        let icon_button = |ui: &mut Ui<'_>, x: f32, glyph: &str, on: bool| -> bool {
            let r = [x, row_y, 30.0, 30.0];
            let hov = ui.hit(r);
            ui.fill(r, 8.0, if on { ui.accent } else { ink }, if on { 0.9 } else if hov { 0.2 } else { 0.12 });
            ui.glyph(x + 15.0, row_y + 15.0, 15.0, glyph, if on { [1.0; 3] } else { ink }, 0.9);
            if hov {
                ui.want_cursor(Cursor::Pointer);
            }
            hov && ui.click
        };
        if icon_button(ui, x, "copy", false) {
            host.copy = Some(to_hex(rgb));
        }
        x += 36.0;
        if icon_button(ui, x, "clipboard-paste", false) {
            host.paste = true;
        }
        x += 36.0;
        if icon_button(ui, x, "pipette", pk.eyedrop) {
            pk.eyedrop = !pk.eyedrop;
        }
        ui.text(panel[0] + 16.0, panel[1] + 308.0, if pk.eyedrop { "Click a colour on the icon" } else { "Type a hex code, paste, or use the eyedropper" }, 11.0, false, ink, 0.5, Align::Left);
        self.apply_picker();
    }

    // ------------------------------------------------------------------------------------------------ the bottom bar

    fn start_build(&mut self) {
        if self.build.is_some() {
            return;
        }
        self.flush();
        let docs = icondoc::saved_docs();
        if docs.is_empty() {
            self.note = "Nothing to build yet: change an icon first.".into();
            return;
        }
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let waker = self.waker.clone();
        let total = docs.len();
        std::thread::spawn(move || {
            let wake = |msg: Msg| {
                let _ = tx.send(msg);
                if let Some(w) = &waker {
                    w.wake();
                }
            };
            let out = iconrender::theme_dir();
            let result = iconrender::build_theme(&docs, SOURCE_THEME, &out, &|| flag.load(Ordering::Relaxed), &|d, t| wake(Msg::Progress(d, t)));
            wake(Msg::Done(result));
        });
        self.build = Some(Build { rx, cancel, done: 0, total });
        self.note.clear();
    }

    fn draw_bar(&mut self, ui: &mut Ui<'_>, bar: [f32; 4]) {
        let (ink, accent) = (ui.ink, ui.accent);
        self.panel_bg(ui, bar);
        ui.fill([bar[0], bar[1], bar[2], 1.0], 0.0, ink, 0.09);
        let w = bar[2];
        if let Some(job) = &self.build {
            let frac = (job.done as f32 / job.total.max(1) as f32).clamp(0.0, 1.0);
            let track = [20.0, bar[1] + 18.0, w - 160.0, 8.0];
            ui.fill(track, 4.0, ink, 0.10);
            ui.fill([track[0], track[1], (track[2] * frac).max(8.0), track[3]], 4.0, accent, 1.0);
            ui.text_raw(20.0, bar[1] + 46.0, &format!("Building the theme… {} of {}", job.done.min(job.total), job.total), 12.5, false, ink, 0.7, Align::Left);
            if ui.button([w - 120.0, bar[1] + 13.0, 100.0, 34.0], "Cancel", false) {
                job.cancel.store(true, Ordering::Relaxed);
            }
            return;
        }
        if ui.icon_button([w - 210.0, bar[1] + 13.0, 190.0, 34.0], "hammer", "Build icon theme", true) {
            self.start_build();
        }
        if self.note.is_empty() {
            let hint = match &self.doc {
                Some(d) => format!("{} is saved as you work, in {}", d.name, icondoc::doc_path(&d.name).display()),
                None => "Pick an icon in the Icons tab".to_string(),
            };
            let shown = fit(ui, &hint, 12.0, false, w - 260.0);
            ui.text_raw(20.0, bar[1] + 35.0, &shown, 12.0, false, ink, 0.5, Align::Left);
        } else {
            let note = self.note.clone();
            let shown = fit(ui, &note, 12.5, false, w - 260.0);
            ui.text_raw(20.0, bar[1] + 35.0, &shown, 12.5, false, ink, 0.8, Align::Left);
        }
    }
}

/// The factors of the glass that have a menu, with the way to read and write each.
fn tune_params() -> [(&'static str, fn(&GlassTune) -> f32, fn(&mut GlassTune, f32)); 9] {
    [
        ("Specular", |t| t.specular, |t, v| t.specular = v),
        ("Blur", |t| t.blur, |t, v| t.blur = v),
        ("Refractivity", |t| t.refraction, |t, v| t.refraction = v),
        ("Translucency", |t| t.translucency, |t, v| t.translucency = v),
        ("Shadow", |t| t.shadow, |t, v| t.shadow = v),
        ("Shadow reach", |t| t.shadow_dist, |t, v| t.shadow_dist = v),
        ("Shadow softness", |t| t.shadow_soft, |t, v| t.shadow_soft = v),
        ("Corners", |t| t.corner, |t, v| t.corner = v),
        ("Sharpness", |t| t.sharp, |t, v| t.sharp = v),
    ]
}

impl Composer {
    /// Where the glass window is, clamped to the window.
    fn glass_rect(&self, win: [f32; 4]) -> [f32; 4] {
        let (w, h) = (340.0, 100.0 + 10.0 * 34.0 + 100.0);
        let x = if self.glass_pos[0] < 0.0 { win[2] - RIGHT_W - w - 14.0 } else { self.glass_pos[0] };
        [x.clamp(0.0, (win[2] - w).max(0.0)), self.glass_pos[1].clamp(0.0, (win[3] - 60.0).max(0.0)), w, h]
    }

    /// The window that floats over the editor with the presets and the sliders of the glass of every icon.
    fn draw_glass_panel(&mut self, ui: &mut Ui<'_>) {
        let (ink, dark) = (ui.ink, ui.dark);
        let r = self.glass_rect(self.win);
        ui.fill([r[0] - 1.0, r[1] - 1.0, r[2] + 2.0, r[3] + 2.0], 15.0, ink, 0.18);
        ui.fill(r, 14.0, card_color(dark), 0.985);
        // the title bar: it carries the window about
        let bar = [r[0], r[1], r[2], 40.0];
        ui.fill([r[0], r[1] + 39.0, r[2], 1.0], 0.0, ink, 0.10);
        ui.glyph(r[0] + 22.0, r[1] + 20.0, 15.0, "sparkles", ink, 0.7);
        ui.text(r[0] + 40.0, r[1] + 25.0, "Glass of every icon", 13.5, true, ink, 0.95, Align::Left);
        let close = [r[0] + r[2] - 34.0, r[1] + 8.0, 24.0, 24.0];
        let close_hov = ui.hit(close);
        if close_hov {
            ui.fill(close, 12.0, ink, 0.12);
            ui.want_cursor(Cursor::Pointer);
        }
        ui.glyph(close[0] + 12.0, close[1] + 12.0, 13.0, "x", ink, 0.8);
        if ui.click && close_hov {
            self.glass_panel = false;
            return;
        }
        if ui.click && ui.hit(bar) && !close_hov {
            self.glass_grab = Some([ui.mouse[0] - r[0], ui.mouse[1] - r[1]]);
        }
        if let Some(g) = self.glass_grab {
            if ui.held {
                self.glass_pos = [ui.mouse[0] - g[0], ui.mouse[1] - g[1]];
            } else {
                self.glass_grab = None;
            }
        }
        // the presets
        let before = self.tune;
        let mut t = before;
        let bw = (r[2] - 32.0 - 4.0 * 4.0) / 5.0;
        for (i, (name, preset)) in GLASS_PRESETS.iter().enumerate() {
            if ui.button([r[0] + 16.0 + i as f32 * (bw + 4.0), r[1] + 52.0, bw, 28.0], name, t == *preset) {
                t = *preset;
            }
        }
        // the sliders
        let mut f = Form::new(r[0] + 16.0, r[1] + 114.0, r[2] - 32.0, 34.0);
        for (label, get, set) in tune_params() {
            let mut pc = get(&t) * 100.0;
            f.slider(ui, label, &mut pc, 0.0, 200.0, "%");
            set(&mut t, pc / 100.0);
        }
        f.slider(ui, "Light from", &mut t.light, 0.0, 360.0, "°");
        // the clear looks in colour
        f.skip(4.0);
        f.toggle(ui, "Clear keeps colours", &mut t.colour_clear);
        let reset = [r[0] + 16.0, r[1] + r[3] - 44.0, r[2] - 32.0, 30.0];
        if ui.icon_button(reset, "rotate-ccw", "Reset the glass", false) {
            t = GlassTune::default();
        }
        if t != before {
            self.apply_tune(t);
        }
    }

    /// The menu bar along the top of the window (only when the shell does not show the menus itself), and the open menu.
    fn draw_menu_strip(&mut self, ui: &mut Ui<'_>, w: f32, host: &mut Host) {
        let (ink, accent) = (ui.ink, ui.accent);
        let strip = [0.0, 0.0, w, MENU_H];
        self.panel_bg(ui, strip);
        ui.fill([0.0, MENU_H - 1.0, w, 1.0], 0.0, ink, 0.10);
        let menus = self.bar_menus();
        let mut x = 8.0;
        let mut titles: Vec<[f32; 4]> = Vec::new();
        for (i, m) in menus.iter().enumerate() {
            let tw = ui.measure(&m.title, 13.0, false) + 22.0;
            let r = [x, 3.0, tw, MENU_H - 6.0];
            titles.push(r);
            let on = self.menu_open == Some(i);
            let hov = ui.hit(r);
            if on {
                ui.fill(r, 7.0, accent, 0.9);
            } else if hov {
                ui.fill(r, 7.0, ink, 0.10);
            }
            ui.text(r[0] + tw / 2.0, r[1] + r[3] / 2.0 + 4.5, &m.title, 13.0, on, if on { [1.0; 3] } else { ink }, 0.95, Align::Center);
            if hov {
                ui.want_cursor(Cursor::Pointer);
                if ui.click {
                    // a press on the open one closes it; with one open, passing over another opens that
                    self.menu_open = if on { None } else { Some(i) };
                    self.menu_sub.clear();
                } else if self.menu_open.is_some() && !on {
                    self.menu_open = Some(i);
                    self.menu_sub.clear();
                }
            }
            x += tw + 2.0;
        }
        let Some(open) = self.menu_open else { return };
        let Some(menu) = menus.get(open) else {
            self.menu_open = None;
            return;
        };
        let mut chosen = None;
        let mut rects = vec![];
        let at = titles[open];
        self.menu_panel(ui, &menu.items, 0, at[0], MENU_H - 2.0, w, &mut chosen, &mut rects);
        if let Some(id) = chosen {
            self.menu_open = None;
            self.menu_sub.clear();
            self.bar_action(id, host);
            host.redraw = true;
        } else if ui.click && !titles.iter().chain(rects.iter()).any(|r| inside(ui.mouse, *r)) {
            self.menu_open = None;
            self.menu_sub.clear();
        }
        host.redraw = true;
    }

    /// One dropdown of the bar (and, below it, the submenu that is open in it). `chosen` gets the id that was pressed.
    #[allow(clippy::too_many_arguments)]
    fn menu_panel(&mut self, ui: &mut Ui<'_>, items: &[Item], depth: usize, x: f32, y: f32, win_w: f32, chosen: &mut Option<u32>, rects: &mut Vec<[f32; 4]>) {
        let (ink, accent, dark) = (ui.ink, ui.accent, ui.dark);
        const ROW: f32 = 26.0;
        const SEP: f32 = 9.0;
        let mut w = 190.0f32;
        for it in items.iter().filter(|i| !i.separator) {
            let key = if it.key.is_empty() { 0.0 } else { ui.measure(&it.key, 11.5, false) + 24.0 };
            w = w.max(ui.measure(&it.label, 13.0, false) + key + 64.0);
        }
        let h: f32 = items.iter().map(|i| if i.separator { SEP } else { ROW }).sum::<f32>() + 12.0;
        let x = x.min(win_w - w - 6.0).max(4.0);
        let panel = [x, y, w, h];
        rects.push(panel);
        ui.fill([x - 1.0, y - 1.0, w + 2.0, h + 2.0], 11.0, ink, 0.16);
        ui.fill(panel, 10.0, card_color(dark), 0.985);
        let mut yy = y + 6.0;
        let mut sub_at = None;
        for (i, it) in items.iter().enumerate() {
            if it.separator {
                ui.fill([x + 10.0, yy + SEP / 2.0, w - 20.0, 1.0], 0.0, ink, 0.12);
                yy += SEP;
                continue;
            }
            let r = [x + 5.0, yy, w - 10.0, ROW];
            let hov = ui.hit(r) && it.enabled;
            let open_sub = self.menu_sub.get(depth) == Some(&i) && !it.sub.is_empty();
            if hov || open_sub {
                ui.fill(r, 6.0, accent, if hov { 0.9 } else { 0.35 });
            }
            let text = if hov { [1.0; 3] } else { ink };
            let a = if it.enabled { 0.95 } else { 0.35 };
            if it.checked {
                ui.glyph(r[0] + 14.0, r[1] + ROW / 2.0, 13.0, "check", text, a);
            }
            ui.text(r[0] + 28.0, r[1] + ROW / 2.0 + 4.5, &it.label, 13.0, false, text, a, Align::Left);
            if !it.sub.is_empty() {
                ui.glyph(r[0] + r[2] - 14.0, r[1] + ROW / 2.0, 13.0, "chevron-right", text, a);
            } else if !it.key.is_empty() {
                ui.text(r[0] + r[2] - 12.0, r[1] + ROW / 2.0 + 4.5, &it.key, 11.5, false, text, a * 0.6, Align::Right);
            }
            if hov {
                // the pointer decides which submenu is open at this depth
                self.menu_sub.truncate(depth);
                if !it.sub.is_empty() {
                    self.menu_sub.push(i);
                }
                if ui.click && it.sub.is_empty() {
                    *chosen = it.id.into();
                }
            }
            if self.menu_sub.get(depth) == Some(&i) && !it.sub.is_empty() {
                sub_at = Some((i, r[1] - 6.0));
            }
            yy += ROW;
        }
        if let Some((i, sy)) = sub_at {
            let sub = items[i].sub.clone();
            self.menu_panel(ui, &sub, depth + 1, x + w - 4.0, sy, win_w, chosen, rects);
        }
    }

    /// The menus of the shell's menu bar: the file, the edit, the glass of every icon and the view.
    fn bar_menus(&self) -> Vec<Menu> {
        let has_doc = self.doc.is_some();
        let has_pick = !self.picked().is_empty();
        let t = self.tune;
        let mut glass: Vec<Item> = GLASS_PRESETS.iter().enumerate().map(|(i, (name, p))| Item::new(100 + i as u32, *name).checked(t == *p)).collect();
        glass.push(Item::sep());
        glass.push(Item::new(431, "Clear keeps the colours").checked(t.colour_clear));
        glass.push(Item::new(430, "Glass settings…").checked(self.glass_panel));
        glass.push(Item::sep());
        glass.push(Item::new(420, "Reset the glass").enabled(t != GlassTune::default()));
        vec![
            Menu::new("File", vec![
                Item::new(1, "Export PNGs").enabled(has_doc),
                Item::new(2, "Build icon theme"),
                Item::sep(),
                if self.apple_icons.is_empty() {
                    Item::new(0, "Import Apple .icon (none on the Desktop or in Downloads)").enabled(false)
                } else {
                    Item::parent("Import Apple .icon", self.apple_icons.iter().enumerate().map(|(k, p)| Item::new(600 + k as u32, p.file_name().map_or(String::new(), |n| n.to_string_lossy().to_string()))).collect())
                },
                Item::sep(),
                Item::new(3, "Close").key("Ctrl+Q"),
            ]),
            Menu::new("Edit", vec![
                Item::new(10, "Undo").key("Ctrl+Z").enabled(!self.undo.is_empty()),
                Item::new(11, "Redo").key("Ctrl+Y").enabled(!self.redo.is_empty()),
                Item::sep(),
                Item::new(12, "Duplicate").enabled(has_pick),
                Item::new(13, "Delete").enabled(has_pick),
                Item::new(14, "Group").enabled(has_pick),
                Item::new(15, "Ungroup").enabled(has_pick),
                Item::sep(),
                Item::new(16, "Flip horizontally").enabled(has_pick),
                Item::new(17, "Flip vertically").enabled(has_pick),
                Item::new(18, "Turn right").enabled(has_pick),
                Item::new(19, "Centre").enabled(has_pick),
                Item::sep(),
                Item::new(20, "Back to original").enabled(has_doc),
                Item::new(21, if self.confirm_reset_all { "Reset all icons: choose again to confirm" } else { "Reset all icons" }),
            ]),
            Menu::new("Glass", glass),
            Menu::new("View", vec![
                Item::new(30, "Zoom in").enabled(self.zoom < 2.0),
                Item::new(31, "Zoom out").enabled(self.zoom > 0.5),
                Item::new(32, "Actual size").enabled((self.zoom - 1.0).abs() > 1e-3),
                Item::sep(),
                Item::new(33, "All looks").checked(self.view == 1),
                Item::new(34, "Flat glass").checked(self.flat),
                Item::new(35, "Live in the shell").checked(self.live),
                Item::sep(),
                Item::parent("Background", ["Dark", "Light", "Colour", "Photo"].iter().enumerate().map(|(i, n)| Item::new(40 + i as u32, *n).checked(self.backdrop == i)).collect()),
                Item::new(44, "Next wallpaper").enabled(self.backdrop == 3),
                Item::new(45, "Previous wallpaper").enabled(self.backdrop == 3),
            ]),
        ]
    }

    fn bar_action(&mut self, id: u32, host: &mut Host) {
        match id {
            1 => self.export_png(),
            2 => self.start_build(),
            3 => host.quit = true,
            10 => self.undo_step(),
            11 => self.redo_step(),
            12 | 13 | 14 | 15 => {
                let picked = self.picked();
                self.layer_action(&picked, match id { 12 => 5, 13 => 4, 14 => 2, _ => 3 });
            }
            16 => self.svg_op(SvgOp::FlipH),
            17 => self.svg_op(SvgOp::FlipV),
            18 => self.svg_op(SvgOp::RotateCw),
            19 => self.centre_picked(),
            20 => self.reset_current(),
            21 => {
                if self.confirm_reset_all {
                    self.reset_all();
                } else {
                    self.confirm_reset_all = true;
                    self.note = "Choose \"Reset all icons\" again to remove every change of every icon.".into();
                }
            }
            30 => self.set_zoom(self.zoom + 0.1),
            31 => self.set_zoom(self.zoom - 0.1),
            32 => self.set_zoom(1.0),
            33 => {
                self.view = 1 - self.view;
                self.cdrag = None;
            }
            34 => {
                self.flat = !self.flat;
                self.version += 1;
                self.thumbs.clear();
            }
            35 => self.live = !self.live,
            40..=43 => {
                self.backdrop = (id - 40) as usize;
                self.backdrop_key = None;
            }
            44 | 45 => {
                if !self.walls.is_empty() {
                    let n = self.walls.len() as i32;
                    self.wall = (self.wall as i32 + if id == 44 { 1 } else { -1 }).rem_euclid(n) as usize;
                }
            }
            100..=199 => {
                if let Some((_, p)) = GLASS_PRESETS.get((id - 100) as usize) {
                    self.apply_tune(*p);
                }
            }
            420 => self.apply_tune(GlassTune::default()),
            430 => self.glass_panel = !self.glass_panel,
            431 => self.apply_tune(GlassTune { colour_clear: !self.tune.colour_clear, ..self.tune }),
            600..=699 => {
                if let Some(p) = self.apple_icons.get((id - 600) as usize).cloned() {
                    self.import_apple(&p);
                }
            }
            _ => return,
        }
        host.redraw = true;
    }
}

impl App for Composer {
    fn menus(&self) -> Vec<Menu> {
        self.bar_menus()
    }

    fn menu_action(&mut self, id: u32, host: &mut Host) {
        self.bar_action(id, host);
    }

    fn start(&mut self, waker: app::Waker) {
        let waker = Arc::new(waker);
        // the icon is drawn on a thread of its own, so that a slider never waits for it
        let (job_tx, job_rx) = channel::<Job>();
        let (done_tx, done_rx) = channel::<Done>();
        let wake = waker.clone();
        std::thread::spawn(move || {
            let mut renderer = Renderer::new();
            while let Ok(first) = job_rx.recv() {
                // only the newest request of each kind is worth drawing
                let (mut one, mut grid) = (None, None);
                for job in std::iter::once(first).chain(job_rx.try_iter()) {
                    if job.grid {
                        grid = Some(job);
                    } else {
                        one = Some(job);
                    }
                }
                if let Some(job) = one {
                    renderer.flat = job.flat;
                    renderer.tune = job.tune;
                    renderer.backdrop = job.backdrop;
                    let img = renderer.render(&job.doc, job.look, job.n);
                    // the small copies are not on the wallpaper, so they are drawn without it
                    let plain = renderer.backdrop.take().map(|_| renderer.render(&job.doc, job.look, 128));
                    if done_tx.send(Done::One { version: job.version, look: job.look, img, plain }).is_err() {
                        break;
                    }
                    wake.wake();
                }
                if let Some(job) = grid {
                    renderer.flat = job.flat;
                    renderer.tune = job.tune;
                    let imgs = GRID_ORDER.iter().enumerate().map(|(k, l)| {
                        renderer.backdrop = job.behind.get(k).cloned();
                        (*l, renderer.render(&job.doc, *l, GRID_N))
                    }).collect();
                    if done_tx.send(Done::Grid { version: job.version, imgs }).is_err() {
                        break;
                    }
                    wake.wake();
                }
            }
        });
        self.jobs = Some(job_tx);
        self.results = Some(done_rx);
        self.waker = Some(waker);
    }

    /// The picker takes the keyboard while it is open: typing in its hex field, Ctrl+C / Ctrl+V / Ctrl+A, Enter and Esc.
    /// Without it Ctrl+Z / Ctrl+Y (Ctrl+Shift+Z) go through the history.
    fn raw_key(&mut self, event: &KeyEvent, mods: &Modifiers, _serial: u32, host: &mut Host) -> bool {
        if self.menu_open.is_none() && self.glass_panel && event.keysym == Keysym::Escape {
            self.glass_panel = false;
            host.redraw = true;
            return true;
        }
        if self.menu_open.is_some() && event.keysym == Keysym::Escape {
            self.menu_open = None;
            self.menu_sub.clear();
            host.redraw = true;
            return true;
        }
        // the shortcut letters are the keys, whatever the layout is (Ctrl+Z is the same key on a Ukrainian layout)
        let letter = match event.raw_code {
            44 => Some('z'),
            21 => Some('y'),
            46 => Some('c'),
            47 => Some('v'),
            30 => Some('a'),
            45 => Some('x'),
            _ => event.keysym.key_char().map(|c| c.to_ascii_lowercase()),
        };
        let Some(pk) = self.picker.as_mut() else {
            // the arrow keys nudge the selected layer
            if let (false, Some(Sel::Node(id))) = (mods.ctrl, self.sel) {
                let step = if mods.shift { 0.02 } else { 0.004 };
                let (dx, dy) = match event.keysym {
                    Keysym::Left => (-step, 0.0),
                    Keysym::Right => (step, 0.0),
                    Keysym::Up => (0.0, -step),
                    Keysym::Down => (0.0, step),
                    _ => (0.0, 0.0),
                };
                if dx != 0.0 || dy != 0.0 {
                    // each nudge is a step of history
                    if let Some(d) = self.doc.clone() {
                        self.undo.push(d);
                        self.redo.clear();
                    }
                    for id in std::iter::once(id).chain(self.also.clone()) {
                        if let Some(mut p) = self.props_of(id) {
                            p.offset = [p.offset[0] + dx, p.offset[1] + dy];
                            self.set_props(id, p);
                        }
                    }
                    return true;
                }
            }
            if mods.ctrl {
                match letter {
                    Some('z') if mods.shift => self.redo_step(),
                    Some('z') => self.undo_step(),
                    Some('y') => self.redo_step(),
                    _ => return false,
                }
                return true;
            }
            return false;
        };
        if mods.ctrl {
            match letter {
                Some('c') | Some('x') => {
                    host.copy = Some(to_hex(hsv_to_rgb(pk.hsv)));
                    return true;
                }
                Some('v') => {
                    host.paste = true;
                    return true;
                }
                Some('a') if pk.focus => {
                    pk.selected = true;
                    return true;
                }
                _ => return pk.focus,
            }
        }
        let edited = match event.keysym {
            Keysym::Escape => {
                if pk.focus {
                    pk.focus = false;
                } else {
                    self.picker = None;
                }
                return true;
            }
            _ if !pk.focus => return false,
            Keysym::Return | Keysym::KP_Enter => {
                pk.focus = false;
                return true;
            }
            Keysym::BackSpace | Keysym::Delete => {
                if pk.selected {
                    pk.hex.clear();
                } else if event.keysym == Keysym::BackSpace {
                    pk.hex.pop();
                }
                pk.selected = false;
                true
            }
            _ => {
                let typed: String = event.utf8.as_deref().unwrap_or("").chars().filter(|c| c.is_ascii_hexdigit() || *c == '#').collect();
                if typed.is_empty() {
                    return true;
                }
                if pk.selected {
                    pk.hex.clear();
                    pk.selected = false;
                }
                for c in typed.chars() {
                    let room = if pk.hex.starts_with('#') { 7 } else { 6 };
                    if pk.hex.len() < room && (c != '#' || pk.hex.is_empty()) {
                        pk.hex.push(c);
                    }
                }
                true
            }
        };
        if edited {
            // the colour follows what is typed as soon as it is a whole code
            if let Some(c) = parse_hex(&pk.hex) {
                pk.hsv = rgb_to_hsv(c);
                self.apply_picker();
            }
        }
        true
    }

    /// The text that Ctrl+V (or the paste button) brought: a colour code goes into the picker.
    fn clipboard(&mut self, text: String, _host: &mut Host) {
        let Some(c) = memrium_de::svgedit::parse_colour(text.split_whitespace().next().unwrap_or("")).or_else(|| parse_hex(text.trim())) else { return };
        if let Some(pk) = self.picker.as_mut() {
            pk.hsv = rgb_to_hsv(c);
            pk.hex = to_hex(c);
            pk.selected = false;
        }
        self.apply_picker();
    }

    fn tick(&mut self, host: &mut Host) {
        if self.live_pending && !self.live_busy.load(Ordering::Acquire) {
            self.live_write(false);
        }
        if self.live_all_due.is_some_and(|t| t.elapsed() >= std::time::Duration::from_millis(700)) && !self.live_busy.load(Ordering::Acquire) {
            self.live_all_due = None;
            self.live_write(true);
        }
        if self.live_all_due.is_some() {
            host.redraw = true;
        }
        let mut finished = None;
        if let Some(job) = &mut self.build {
            while let Ok(m) = job.rx.try_recv() {
                match m {
                    Msg::Progress(d, t) => {
                        job.done = d;
                        job.total = t;
                    }
                    Msg::Done(r) => finished = Some(r),
                }
            }
            host.redraw = true;
        }
        if let Some(r) = finished {
            self.build = None;
            self.note = match r {
                Ok(n) => format!("Done: {n} pictures in {}. Choose Memrium-Icons in Settings → Icons, with shape and plate set to None.", iconrender::theme_dir().display()),
                Err(e) if e == "cancelled" => "Cancelled".into(),
                Err(e) => format!("Could not build the theme: {e}"),
            };
        }
        // a picture is on its way: look again when it has arrived
        let waiting = self.sent.is_some_and(|(v, l)| self.shown.as_ref().map_or(true, |s| s.version != v || s.look != l))
            || (self.view == 1 && self.grid_sent.is_some_and(|v| self.grid.as_ref().map_or(true, |g| g.0 != v)));
        if waiting && self.results.is_some() {
            host.redraw = true;
        }
    }

    fn draw(&mut self, ui: &mut Ui<'_>, size: [f32; 2], host: &mut Host) {
        let [w, h] = size;
        self.win = [0.0, 0.0, w, h];
        if self.backdrop == 3 {
            self.ensure_wall();
        }
        ui.fill([0.0, 0.0, w, h], 0.0, page_color(ui.dark), 1.0);
        if self.photo() {
            self.paint_wall(ui, false, None);
        }
        // the shell shows the menus when it can; else they are along the top of this window
        let top = if host.menu_bar { 0.0 } else { MENU_H };
        let body_h = h - BAR_H - top;
        self.poll();
        // the document as it was when this press began, for Undo
        if ui.click {
            self.pending = self.doc.clone();
        }
        // while the picker is open (or the eyedropper is on), what is under it takes no clicks
        let panel = self.picker_rect(size);
        let (mouse, click, scroll) = (ui.mouse, ui.click, ui.scroll);
        // an open menu takes the clicks: what is under it waits
        let menu_modal = self.menu_open.is_some();
        let glass_rect = self.glass_panel.then(|| self.glass_rect(self.win));
        let over_glass = glass_rect.is_some_and(|g| inside(mouse, g)) || self.glass_grab.is_some();
        let over_picker = panel.is_some_and(|p| inside(mouse, p)) || menu_modal || over_glass;
        let pick_mode = self.picker.as_ref().is_some_and(|p| p.eyedrop);
        let block = |ui: &mut Ui<'_>, on: bool| {
            if on {
                ui.mouse = [-1000.0; 2];
                ui.click = false;
                ui.scroll = 0.0;
            } else {
                ui.mouse = mouse;
                ui.click = click;
                ui.scroll = scroll;
            }
        };
        block(ui, over_picker || pick_mode);
        self.draw_left(ui, [0.0, top, LEFT_W, body_h]);
        self.draw_inspector(ui, [w - RIGHT_W, top, RIGHT_W, body_h]);
        block(ui, over_picker);
        self.draw_stage(ui, [LEFT_W, top, w - LEFT_W - RIGHT_W, body_h]);
        block(ui, over_picker || pick_mode);
        self.draw_bar(ui, [0.0, top + body_h, w, BAR_H]);
        block(ui, false);
        if self.glass_panel {
            // while a menu is open the window is only drawn: it takes nothing
            let (m, c) = (ui.mouse, ui.click);
            if menu_modal {
                ui.mouse = [-1000.0; 2];
                ui.click = false;
            }
            self.draw_glass_panel(ui);
            ui.mouse = m;
            ui.click = c;
        }
        if !host.menu_bar {
            self.draw_menu_strip(ui, w, host);
        }
        if let Some(panel) = self.picker_rect(size) {
            self.draw_picker(ui, panel, host);
        }
        // a press that changed the document is one step of history
        if !ui.held {
            if let Some(before) = self.pending.take() {
                if self.doc.as_ref().is_some_and(|d| *d != before) {
                    self.undo.push(before);
                    if self.undo.len() > 100 {
                        self.undo.remove(0);
                    }
                    self.redo.clear();
                }
            }
            // saved once the pointer is up, so that dragging a slider writes the file once
            self.flush();
        }
    }
}

fn main() {
    let first = std::env::args().nth(1);
    let mut composer = Composer::new(first);
    // the photo behind everything, as the glass is seen best over a picture
    composer.backdrop = 3;
    app::run(Config::new("Memrium Icons", "memrium-icons").size(1280, 700).min_size(1240, 600), composer);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chebrium::canvas::Canvas;
    use chebrium::soft::{Gfx, SoftDraw};
    use chebrium::style::Style;
    use chebrium::ui::State;

    const W: usize = 1280;
    const H: usize = 700;

    /// The program drawn without a window, fed with a pointer the way the runtime does it.
    /// the tests share one folder of documents (and the environment), so one runs at a time
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct Rig {
        _one: std::sync::MutexGuard<'static, ()>,
        app: Composer,
        state: State,
        gfx: Gfx,
        host: Host,
    }

    impl Rig {
        fn new() -> Rig {
            let one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
            // documents are saved: keep that away from the real ones
            let dir = std::env::temp_dir().join(format!("memrium-icons-test-{}", std::process::id()));
            std::env::set_var("XDG_DATA_HOME", &dir);
            // what an earlier test saved is not there
            let _ = std::fs::remove_dir_all(icondoc::docs_dir());
            let mut app = Composer::new(Some("firefox".into()));
            // not the glass that another test left in the shared config
            app.tune = GlassTune::default();
            // no icon theme is written in the background while a test runs
            app.live = false;
            app.left_tab = 0;
            let mut r = Rig { _one: one, app, state: State::default(), gfx: Gfx::new(), host: Host::default() };
            r.frame();
            r
        }

        fn frame(&mut self) {
            let mut buf = vec![0u8; W * H * 4];
            let mut canvas = Canvas::new(&mut buf, W, H);
            let mut painter = SoftDraw::new(&mut canvas, &mut self.gfx);
            let dt = self.state.begin_frame();
            let mut ui = Ui::new(&mut painter, &mut self.state, Style::new(true, [0.23, 0.55, 1.0]), dt);
            self.app.draw(&mut ui, [W as f32, H as f32], &mut self.host);
            let seen = ui.edit_seen;
            self.state.end_frame(seen);
            if let Ok(path) = std::env::var("TMP_SHOT") {
                let mut rgba = buf.clone();
                for px in rgba.chunks_exact_mut(4) {
                    px.swap(0, 2);
                }
                image::save_buffer(path, &rgba, W as u32, H as u32, image::ColorType::Rgba8).unwrap();
            }
        }

        /// Move there, press, let go, with a frame after each step.
        fn click(&mut self, p: [f32; 2]) {
            self.state.pointer_move(p);
            self.frame();
            self.state.pointer_down(p);
            self.frame();
            self.state.pointer_up();
            self.frame();
        }

        fn key(&mut self, keysym: Keysym, text: &str, ctrl: bool) -> bool {
            let event = KeyEvent { time: 0, raw_code: 0, keysym, utf8: if text.is_empty() { None } else { Some(text.to_string()) } };
            let mods = Modifiers { ctrl, ..Modifiers::default() };
            let used = self.app.raw_key(&event, &mods, 0, &mut self.host);
            self.frame();
            used
        }

        fn center_of_row(&self, i: usize) -> [f32; 2] {
            let r = self.app.rows[i].1;
            [r[0] + r[2] / 2.0 - 20.0, r[1] + r[3] / 2.0]
        }
    }

    fn alpha_sum(img: &image::RgbaImage) -> u64 {
        img.pixels().map(|p| p[3] as u64).sum()
    }

    #[test]
    fn an_icon_opens_as_layers_and_is_drawn() {
        let r = Rig::new();
        let doc = r.app.doc.as_ref().expect("firefox opens");
        assert!(doc.layers().len() > 1, "Papirus firefox has several shapes");
        assert!(!r.app.rows.is_empty(), "the layer list shows them");
        let shown = r.app.shown.as_ref().expect("the icon is drawn");
        assert!(alpha_sum(&shown.img) > 10_000);
    }

    #[test]
    fn a_click_on_the_icon_selects_the_layer_under_it_and_the_list_follows() {
        let mut r = Rig::new();
        let rect = r.app.big_rect;
        r.click([rect[0] + rect[2] * 0.5, rect[1] + rect[3] * 0.6]);
        let Some(Sel::Node(id)) = r.app.sel else { panic!("a layer is selected") };
        assert!(r.app.doc.as_ref().unwrap().find(id).is_some());
        // clicking a row selects it too
        let second = r.app.rows[1].0;
        let p = r.center_of_row(1);
        r.click(p);
        assert_eq!(r.app.sel, Some(Sel::Node(second)));
        // and a click on empty stage selects the plate
        r.click([rect[0] + 3.0, rect[1] + 3.0]);
        assert_eq!(r.app.sel, Some(Sel::Background));
    }

    #[test]
    fn glass_and_opacity_change_the_picture_and_undo_brings_it_back() {
        let mut r = Rig::new();
        let id = r.app.doc.as_ref().unwrap().layers()[2].id;
        let before = r.app.shown.as_ref().unwrap().img.clone();
        let mut p = r.app.props_of(id).unwrap();
        p.opacity = 0.3;
        p.glass.enabled = true;
        r.app.set_props(id, p);
        r.frame();
        let after = r.app.shown.as_ref().unwrap().img.clone();
        assert_ne!(before.as_raw(), after.as_raw(), "the picture follows the settings");

        // delete the selected layer with the button, then Undo
        let p = r.center_of_row(0);
        r.click(p);
        let count = r.app.doc.as_ref().unwrap().layers().len();
        r.click([130.0, 614.0]);
        assert_eq!(r.app.doc.as_ref().unwrap().layers().len(), count - 1, "Delete removed a layer");
        assert!(r.key(Keysym::z, "z", true), "Ctrl+Z is taken");
        assert_eq!(r.app.doc.as_ref().unwrap().layers().len(), count, "Undo brought it back");
        assert!(r.key(Keysym::y, "y", true));
        assert_eq!(r.app.doc.as_ref().unwrap().layers().len(), count - 1, "Redo took it away again");
    }

    #[test]
    fn the_dark_look_is_edited_on_its_own() {
        let mut r = Rig::new();
        let id = r.app.doc.as_ref().unwrap().layers()[1].id;
        r.app.look = Appearance::Dark;
        let mut p = r.app.props_of(id).unwrap();
        p.visible = false;
        r.app.set_props(id, p);
        r.frame();
        let doc = r.app.doc.as_ref().unwrap();
        assert!(doc.find(id).unwrap().props().get(Appearance::Default).visible, "the default look is untouched");
        assert!(!doc.find(id).unwrap().props().get(Appearance::Dark).visible);
        assert!(r.app.shown.as_ref().unwrap().look == Appearance::Dark);
    }

    #[test]
    fn the_document_is_saved_in_the_memicon_format_and_opens_again() {
        let mut r = Rig::new();
        let id = r.app.doc.as_ref().unwrap().layers()[0].id;
        let mut p = r.app.props_of(id).unwrap();
        p.fill = Some([0.9, 0.1, 0.2]);
        r.app.set_props(id, p);
        r.frame();
        let path = icondoc::doc_path("firefox");
        assert!(path.exists(), "saved when the change was done: {}", path.display());
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..8], icondoc::MAGIC);
        let back = icondoc::load(&path).unwrap();
        assert_eq!(&back, r.app.doc.as_ref().unwrap());
        // opening the icon again takes the saved document, not the SVG
        r.app.open("firefox");
        assert_eq!(r.app.props_of(id).unwrap().fill, Some([0.9, 0.1, 0.2]));
    }

    #[test]
    fn the_colour_picker_sets_the_fill_of_the_selected_layer() {
        let mut r = Rig::new();
        let id = r.app.doc.as_ref().unwrap().layers()[0].id;
        r.app.sel = Some(Sel::Node(id));
        r.app.open_picker(PickTarget::LayerFill, [0.5, 0.5, 0.5], 330.0);
        r.frame();
        let panel = r.app.picker_rect([W as f32, H as f32]).unwrap();
        let sv = [panel[0] + 16.0, panel[1] + 16.0, 260.0, 172.0];
        r.click([sv[0] + sv[2] * 0.9, sv[1] + sv[3] * 0.1]);
        let fill = r.app.props_of(id).unwrap().fill.expect("the layer got a fill");
        assert!(fill.iter().any(|v| *v > 0.7), "{fill:?}");
        // typing a code in the hex field
        r.click([panel[0] + 16.0 + 50.0, panel[1] + 256.0 + 15.0]);
        assert!(r.app.picker.as_ref().unwrap().focus);
        for (sym, c) in [(Keysym::f, "f"), (Keysym::f, "f"), (Keysym::_0, "0"), (Keysym::_0, "0"), (Keysym::_0, "0"), (Keysym::_0, "0")] {
            r.key(sym, c, false);
        }
        let fill = r.app.props_of(id).unwrap().fill.unwrap();
        assert!(fill[0] > 0.99 && fill[1] < 0.01 && fill[2] < 0.01, "typed red: {fill:?}");
        // Ctrl+V takes a code from the clipboard, Esc closes the picker
        r.app.clipboard("#00ff00".into(), &mut r.host);
        assert!(r.app.props_of(id).unwrap().fill.unwrap()[1] > 0.99);
        r.key(Keysym::Return, "", false);
        r.key(Keysym::Escape, "", false);
        assert!(r.app.picker.is_none());
    }

    #[test]
    fn the_eyedropper_takes_the_colour_from_the_icon() {
        let mut r = Rig::new();
        let id = r.app.doc.as_ref().unwrap().layers()[0].id;
        r.app.sel = Some(Sel::Node(id));
        r.app.open_picker(PickTarget::LayerFill, [0.0; 3], 330.0);
        r.app.picker.as_mut().unwrap().eyedrop = true;
        r.frame();
        // a picture of a known colour in place of the icon
        let red = image::RgbaImage::from_pixel(BIG, BIG, image::Rgba([230, 20, 20, 255]));
        let shown = r.app.shown.as_mut().unwrap();
        shown.img = red;
        let rect = r.app.big_rect;
        r.click([rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0]);
        let fill = r.app.props_of(id).unwrap().fill.expect("the colour was taken");
        assert!(fill[0] > 0.85 && fill[1] < 0.15, "{fill:?}");
        assert!(r.app.picker.is_some() && !r.app.picker.as_ref().unwrap().eyedrop, "the picker stays open, the eyedropper is done");
    }

    /// Press on a row, move to (x, y), let go: the way a layer is dragged.
    fn drag_row(r: &mut Rig, from: usize, to: [f32; 2]) {
        let start = r.center_of_row(from);
        r.state.pointer_move(start);
        r.frame();
        r.state.pointer_down(start);
        r.frame();
        for k in 1..=4 {
            let t = k as f32 / 4.0;
            r.state.pointer_move([start[0] + (to[0] - start[0]) * t, start[1] + (to[1] - start[1]) * t]);
            r.frame();
        }
        r.state.pointer_up();
        r.frame();
        r.frame();
    }

    #[test]
    fn a_layer_is_dragged_in_the_list_to_reorder_and_to_group() {
        let mut r = Rig::new();
        let front_to_back = |r: &Rig| r.app.rows.iter().map(|(id, _)| *id).collect::<Vec<u32>>();
        let before = front_to_back(&r);
        assert!(before.len() > 5);
        // the layer in the 4th row, dropped on the upper edge of the first row: it becomes the frontmost
        let moved = before[3];
        let top = r.app.rows[0].1;
        drag_row(&mut r, 3, [top[0] + 60.0, top[1] + 3.0]);
        let after = front_to_back(&r);
        assert_eq!(after[0], moved, "now at the front: {after:?}");
        assert_eq!(after.len(), before.len());

        // a layer dropped on the middle of another one is grouped with it
        let (a, b) = (r.app.rows[4].0, r.app.rows[2].0);
        let mid = r.app.rows[2].1;
        drag_row(&mut r, 4, [mid[0] + 60.0, mid[1] + mid[3] / 2.0]);
        let doc = r.app.doc.as_ref().unwrap();
        let group = doc.nodes.iter().find(|n| matches!(n.kind, Kind::Group(_)) && n.children().iter().any(|c| c.id == a) && n.children().iter().any(|c| c.id == b));
        assert!(group.is_some(), "a and b are in a new group");
        assert_eq!(r.app.sel, Some(Sel::Node(a)));

        // a layer dropped on the group goes into it; Undo takes the last drag back
        let gid = group.unwrap().id;
        let row_of_group = r.app.rows.iter().position(|(id, _)| *id == gid).unwrap();
        let outside = r.app.rows.iter().position(|(id, _)| ![gid, a, b].contains(id)).unwrap();
        let outside_id = r.app.rows[outside].0;
        let g = r.app.rows[row_of_group].1;
        drag_row(&mut r, outside, [g[0] + 60.0, g[1] + g[3] * 0.6]);
        assert!(r.app.doc.as_ref().unwrap().find(gid).unwrap().children().iter().any(|c| c.id == outside_id), "dropped into the group");
        assert!(r.key(Keysym::z, "z", true));
        assert!(!r.app.doc.as_ref().unwrap().find(gid).unwrap().children().iter().any(|c| c.id == outside_id), "undone");
    }

    /// Press at `from`, move to `to` in a few steps, let go.
    fn drag_to(r: &mut Rig, from: [f32; 2], to: [f32; 2]) {
        r.state.pointer_move(from);
        r.frame();
        r.state.pointer_down(from);
        r.frame();
        for k in 1..=4 {
            let t = k as f32 / 4.0;
            r.state.pointer_move([from[0] + (to[0] - from[0]) * t, from[1] + (to[1] - from[1]) * t]);
            r.frame();
        }
        r.state.pointer_up();
        r.frame();
        r.frame();
    }

    #[test]
    fn a_layer_is_moved_by_dragging_it_on_the_icon_and_undone_with_ctrl_z_on_any_layout() {
        let mut r = Rig::new();
        let rect = r.app.big_rect;
        let at = [rect[0] + rect[2] * 0.5, rect[1] + rect[3] * 0.6];
        r.click(at);
        let Some(Sel::Node(id)) = r.app.sel else { panic!("a layer is selected") };
        let start = r.app.props_of(id).unwrap().offset;
        drag_to(&mut r, at, [at[0] + 40.0, at[1] - 20.0]);
        let moved = r.app.props_of(id).unwrap().offset;
        assert!(moved[0] - start[0] > 0.1 && start[1] - moved[1] > 0.04, "moved right and up: {start:?} -> {moved:?}");

        // Ctrl+Z on a Ukrainian layout: the key is the same (evdev 44), the letter is not a "z"
        let event = KeyEvent { time: 0, raw_code: 44, keysym: Keysym::Cyrillic_ya, utf8: None };
        let used = r.app.raw_key(&event, &Modifiers { ctrl: true, ..Modifiers::default() }, 0, &mut r.host);
        r.frame();
        assert!(used, "the shortcut is taken");
        assert_eq!(r.app.props_of(id).unwrap().offset, start, "the move is undone as one step");
    }

    #[test]
    fn a_corner_handle_scales_and_the_arrow_keys_nudge() {
        let mut r = Rig::new();
        let rect = r.app.big_rect;
        r.click([rect[0] + rect[2] * 0.5, rect[1] + rect[3] * 0.6]);
        let Some(Sel::Node(id)) = r.app.sel else { panic!("a layer is selected") };
        r.frame();
        let b = r.app.sel_box.expect("the selection has a box");
        // the bottom right corner handle, pulled away from the centre of the icon
        let corner = [rect[0] + b[0] + b[2], rect[1] + b[1] + b[3]];
        let before = r.app.props_of(id).unwrap().scale;
        drag_to(&mut r, corner, [corner[0] + 30.0, corner[1] + 30.0]);
        assert!(r.app.props_of(id).unwrap().scale > before + 0.05, "scaled up: {before} -> {}", r.app.props_of(id).unwrap().scale);

        let x0 = r.app.props_of(id).unwrap().offset[0];
        assert!(r.key(Keysym::Right, "", false));
        assert!(r.app.props_of(id).unwrap().offset[0] > x0, "the arrow key nudged it");
        assert!(r.key(Keysym::z, "z", true));
        assert_eq!(r.app.props_of(id).unwrap().offset[0], x0, "and Undo took the nudge back");
    }

    #[test]
    fn the_overview_shows_all_six_looks_live_and_a_click_picks_one() {
        let mut r = Rig::new();
        r.app.view = 1;
        r.frame();
        r.frame();
        let (_, pics) = r.app.grid.as_ref().expect("the six looks are drawn");
        assert_eq!(pics.len(), 6);
        assert_eq!(r.app.grid_rects.len(), 6);
        // a change in the document is drawn again in the overview
        let first = r.app.grid.as_ref().unwrap().0;
        let id = r.app.doc.as_ref().unwrap().layers()[0].id;
        let mut p = r.app.props_of(id).unwrap();
        p.visible = false;
        r.app.set_props(id, p);
        r.frame();
        r.frame();
        assert!(r.app.grid.as_ref().unwrap().0 > first, "redrawn after the change");
        // a click on the last one makes it the look being edited and goes back to the editor
        let (look, rect) = r.app.grid_rects[5];
        assert_eq!(look, Appearance::TintedDark);
        r.click([rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0]);
        assert_eq!(r.app.look, Appearance::TintedDark);
        assert_eq!(r.app.view, 0);
    }

    #[test]
    fn shift_and_ctrl_pick_several_layers_that_then_move_and_go_together() {
        let mut r = Rig::new();
        let ids: Vec<u32> = (0..4).map(|i| r.app.rows[i].0).collect();
        let p = r.center_of_row(0);
        r.click(p);
        // Shift takes the stretch up to the row, Ctrl takes one out
        r.state.mods.shift = true;
        let p = r.center_of_row(2);
        r.click(p);
        r.state.mods.shift = false;
        let mut got = r.app.picked();
        got.sort();
        let mut want = ids[..3].to_vec();
        want.sort();
        assert_eq!(got, want, "Shift picked the three rows");
        r.state.mods.ctrl = true;
        let p = r.center_of_row(1);
        r.click(p);
        r.state.mods.ctrl = false;
        assert_eq!(r.app.picked().len(), 2, "Ctrl took the middle one out");
        assert!(!r.app.picked().contains(&ids[1]));
        // the arrow keys nudge both
        let before: Vec<f32> = [ids[0], ids[2]].iter().map(|&i| r.app.props_of(i).unwrap().offset[0]).collect();
        assert!(r.key(Keysym::Right, "", false));
        for (k, &i) in [ids[0], ids[2]].iter().enumerate() {
            assert!(r.app.props_of(i).unwrap().offset[0] > before[k], "a picked layer moved");
        }
        assert_eq!(r.app.props_of(ids[1]).unwrap().offset[0], 0.0, "the one left out stayed");
        // Delete takes them all
        let count = r.app.doc.as_ref().unwrap().layers().len();
        r.click([130.0, 614.0]);
        assert_eq!(r.app.doc.as_ref().unwrap().layers().len(), count - 2);
        assert!(r.app.picked().is_empty());
    }

    #[test]
    fn the_looks_follow_each_other_so_one_icon_is_enough() {
        let mut r = Rig::new();
        let id = r.app.doc.as_ref().unwrap().layers()[0].id;
        let edit = |r: &mut Rig, look: Appearance, f: &dyn Fn(&mut Props)| {
            r.app.look = look;
            let mut p = r.app.props_of(id).unwrap();
            f(&mut p);
            r.app.set_props(id, p);
        };
        // moved once, in the default look: every look has it
        edit(&mut r, Appearance::Default, &|p| p.offset = [0.1, 0.0]);
        for look in Appearance::ALL {
            r.app.look = look;
            assert_eq!(r.app.props_of(id).unwrap().offset, [0.1, 0.0], "{look:?} follows the default look");
        }
        // the dark look changes the opacity; moved again in the default look, dark keeps its opacity and follows the move
        edit(&mut r, Appearance::Dark, &|p| p.opacity = 0.5);
        edit(&mut r, Appearance::Default, &|p| p.offset = [0.2, 0.0]);
        r.app.look = Appearance::Dark;
        let p = r.app.props_of(id).unwrap();
        assert_eq!((p.offset, p.opacity), ([0.2, 0.0], 0.5));
        // clear (dark) follows clear (light)
        edit(&mut r, Appearance::ClearLight, &|p| p.visible = false);
        r.app.look = Appearance::ClearDark;
        assert!(!r.app.props_of(id).unwrap().visible);
        r.app.look = Appearance::TintedDark;
        assert!(r.app.props_of(id).unwrap().visible, "the tinted looks are another branch");
        // a change in the overview's document reaches all six drawings
        r.app.view = 1;
        r.frame();
        r.frame();
        assert_eq!(r.app.grid.as_ref().unwrap().1.len(), 6);
    }

    #[test]
    fn zoom_resizes_the_icon_and_the_picture_follows() {
        let mut r = Rig::new();
        let before = r.app.big_rect[2];
        r.app.set_zoom(1.5);
        r.frame();
        r.frame();
        assert!(r.app.big_rect[2] > before * 1.4, "the icon is bigger on the stage");
        assert_eq!(r.app.shown.as_ref().unwrap().img.width(), r.app.n(), "and drawn at that size");
        r.app.set_zoom(0.1);
        assert_eq!(r.app.zoom, 0.5, "there is a smallest size");
    }

    #[test]
    fn a_layer_can_be_flipped_and_the_icon_goes_back_to_the_original() {
        let mut r = Rig::new();
        let first = r.app.doc.as_ref().unwrap().layers()[0].id;
        let svg_of = |r: &Rig| match &r.app.doc.as_ref().unwrap().find(first).unwrap().kind {
            Kind::Layer(l) => l.svg.clone(),
            _ => unreachable!(),
        };
        let original = svg_of(&r);
        r.app.sel = Some(Sel::Node(first));
        r.app.svg_op(SvgOp::FlipH);
        assert_ne!(svg_of(&r), original, "the drawing was flipped");
        r.app.reset_current();
        assert_eq!(svg_of(&r), original, "back to the original");
        r.app.undo_step();
        assert_ne!(svg_of(&r), original, "and Undo brings the flip back");
    }

    #[test]
    fn the_photo_is_the_ground_and_the_glass_is_drawn_over_it() {
        let mut r = Rig::new();
        // not whatever glass another test saved in the shared config
        r.app.tune = GlassTune::default();
        r.app.backdrop = 3;
        for _ in 0..4 {
            r.frame();
        }
        if r.app.walls.is_empty() {
            return;
        }
        assert!(r.app.photo(), "a wallpaper was loaded");
        assert!(r.app.behind.is_some(), "the icon has the photo under it");
        r.app.look = Appearance::ClearLight;
        r.app.version += 1;
        for _ in 0..3 {
            r.frame();
        }
        let img = &r.app.shown.as_ref().unwrap().img;
        let clear = img.pixels().filter(|p| p[3] < 255).count();
        assert!(clear < img.pixels().len() / 10, "the picture holds the photo ({clear} of {} pixels are see-through)", img.pixels().len());
        r.app.set_zoom(1.0);
        r.app.view = 1;
        for _ in 0..4 {
            r.frame();
        }
        assert!(r.app.grid_behind.as_ref().is_some_and(|b| b.len() == 6), "each look of the overview has its own piece of the photo");
    }

    #[test]
    fn the_glass_is_set_from_the_menu_bar() {
        let mut r = Rig::new();
        // keep the saved glass settings of the real user out of this
        std::env::set_var("XDG_CONFIG_HOME", std::env::temp_dir().join(format!("memrium-icons-cfg-{}", std::process::id())));
        let glass = r.app.menus().into_iter().find(|m| m.title == "Glass").expect("a Glass menu");
        assert!(glass.items.iter().any(|i| i.label == "Frosted"), "the presets are in it");
        let before = r.app.tune;
        let mut host = Host::default();
        // the sliders are in a window that the menu opens
        r.app.menu_action(430, &mut host);
        assert!(r.app.glass_panel);
        r.frame();
        r.frame();
        if let Ok(path) = std::env::var("GLASS_SHOT") {
            std::env::set_var("TMP_SHOT", path);
            r.frame();
            std::env::remove_var("TMP_SHOT");
        }
        // a preset
        r.app.menu_action(100 + 1, &mut host);
        assert_eq!(r.app.tune, GLASS_PRESETS[1].1);
        let glass = r.app.menus().into_iter().find(|m| m.title == "Glass").unwrap();
        assert!(glass.items.iter().find(|i| i.label == "Frosted").unwrap().checked, "and it shows as chosen");
        r.app.menu_action(420, &mut host);
        assert_eq!(r.app.tune, GlassTune::default());
    }

    #[test]
    fn without_a_shell_the_menus_are_along_the_top_of_the_window() {
        let mut r = Rig::new();
        std::env::set_var("XDG_CONFIG_HOME", std::env::temp_dir().join(format!("memrium-icons-cfg2-{}", std::process::id())));
        assert!(!r.host.menu_bar);
        // the Glass title is the third one
        let glass_x = {
            let mut x = 8.0;
            let mut at = 0.0;
            let mut gfx_ui = r.app.bar_menus();
            let _ = &mut gfx_ui;
            for (i, m) in r.app.bar_menus().iter().enumerate() {
                let w = m.title.chars().count() as f32 * 7.5 + 22.0;
                if i == 2 {
                    at = x + w / 2.0;
                }
                x += w + 2.0;
            }
            at
        };
        r.click([glass_x, 15.0]);
        assert_eq!(r.app.menu_open, Some(2), "a press on the title opens the menu");
        // a picture of the open menu with a submenu, when asked for
        if let Ok(path) = std::env::var("MENU_SHOT") {
            r.state.pointer_move([glass_x, 30.0 + 6.0 + 5.0 * 26.0 + 9.0 + 13.0 + 26.0]);
            r.frame();
            std::env::set_var("TMP_SHOT", path);
            r.frame();
            std::env::remove_var("TMP_SHOT");
        }
        // the first row of the Glass menu is the preset "Liquid"; "Frosted" is the second
        r.state.pointer_move([glass_x, 30.0 + 6.0 + 26.0 + 13.0]);
        r.frame();
        let before = r.app.tune;
        r.state.pointer_down([glass_x, 30.0 + 6.0 + 26.0 + 13.0]);
        r.frame();
        r.state.pointer_up();
        r.frame();
        assert!(r.app.menu_open.is_none(), "and a choice closes it");
        assert_ne!(r.app.tune, before, "after applying the preset");
        assert_eq!(r.app.tune, GLASS_PRESETS[1].1);
        // Escape closes an open menu
        r.click([glass_x, 15.0]);
        assert!(r.key(Keysym::Escape, "", false));
        assert!(r.app.menu_open.is_none());
    }

    #[test]
    fn an_apple_icon_folder_becomes_a_document() {
        let mut r = Rig::new();
        let dir = std::env::temp_dir().join(format!("apple-{}/Spot.icon", std::process::id()));
        std::fs::create_dir_all(dir.join("Assets")).unwrap();
        std::fs::write(dir.join("Assets/Dot.svg"), r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024"><circle cx="512" cy="512" r="300" fill="#ff0000"/></svg>"##).unwrap();
        std::fs::write(
            dir.join("icon.json"),
            r#"{"fill":{"automatic-gradient":"srgb:0.1,0.3,0.9,1"},"groups":[{"layers":[{"image-name":"Dot.svg","name":"Dot"}],"specular":true}]}"#,
        )
        .unwrap();
        r.app.import_apple(&dir);
        let doc = r.app.doc.as_ref().unwrap();
        assert_eq!(doc.name, "Spot");
        assert_eq!(doc.layers().len(), 1);
        r.frame();
        r.frame();
        assert!(alpha_sum(&r.app.shown.as_ref().unwrap().img) > 10_000, "and it is drawn");
        r.app.undo_step();
        assert_eq!(r.app.doc.as_ref().unwrap().name, "firefox", "Undo brings the open icon back");
    }

    #[test]
    fn a_layer_is_painted_with_a_gradient_from_the_inspector() {
        let mut r = Rig::new();
        let id = r.app.doc.as_ref().unwrap().layers()[0].id;
        r.app.sel = Some(Sel::Node(id));
        r.frame();
        let mut p = r.app.props_of(id).unwrap();
        p.gradient = Some(Gradient { a: [1.0, 0.0, 0.0], b: [0.0, 0.0, 1.0], angle: 90.0, radial: false });
        p.glass = Glass { enabled: true, bevel: 0.8, inner_shadow: 0.3, glow: 0.2, ..Glass::default() };
        r.app.set_props(id, p);
        if let Ok(path) = std::env::var("GRAD_SHOT") {
            std::env::set_var("TMP_SHOT", path);
            r.frame();
            r.frame();
            std::env::remove_var("TMP_SHOT");
        } else {
            r.frame();
            r.frame();
        }
        let back = r.app.props_of(id).unwrap();
        assert_eq!(back.gradient.map(|g| g.angle), Some(90.0));
        assert!(back.glass.bevel > 0.7);
        // and it is saved with the document
        r.app.flush();
        let loaded = icondoc::load(&icondoc::doc_path("firefox")).unwrap();
        let Kind::Layer(l) = &loaded.find(id).unwrap().kind else { panic!() };
        assert!(l.props.default.gradient.is_some());
    }
}
