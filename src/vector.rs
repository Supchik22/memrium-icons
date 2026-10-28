//! The vector editor of the icon composer: draw shapes from scratch and edit paths, in the manner of Inkscape.
//!
//! A drawing is a list of shapes (rectangle, ellipse, bezier path) with a style each. It is stored in the SVG of one layer of
//! the icon: the shapes are written as plain SVG elements (so every renderer of the program draws them) and the model
//! itself is kept in a `<metadata id="memvec">` element, so that a drawing opens again exactly as it was left. An SVG that
//! was not made here is read through `usvg` and turned into paths.
//!
//! The canvas is `SIZE` x `SIZE` units. The editor draws in two pictures: the drawing (rendered by resvg when it changes)
//! and the overlay (selection, nodes, handles, rubber bands), which is rebuilt every frame.

use chebrium::app::{Keysym, Modifiers};
use chebrium::appmenu::Item;
use chebrium::draw::Align;
use chebrium::picture::Pic;
use chebrium::ui::{inside, Cursor, Ui};
use memrium_de::colour::{hsv_to_rgb, to_hex};
use memrium_de::icondoc::Blobs;
use resvg::{tiny_skia, usvg};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;

pub const SIZE: f32 = 512.0;

type P = [f32; 2];
type C = [f32; 3];

// ------------------------------------------------------------------------------------------------------ the model

/// A point of a path with its two bezier handles (offsets from the point; zero: none).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub p: P,
    pub i: P,
    pub o: P,
    /// the handles stay in line when one is moved
    pub smooth: bool,
}

impl Node {
    fn corner(p: P) -> Node {
        Node { p, i: [0.0; 2], o: [0.0; 2], smooth: false }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sub {
    pub nodes: Vec<Node>,
    pub closed: bool,
}

impl Sub {
    fn seg_count(&self) -> usize {
        match self.nodes.len() {
            0 | 1 => 0,
            n if self.closed => n,
            n => n - 1,
        }
    }

    /// The four bezier points of segment `i` (from node i to the next).
    fn seg(&self, i: usize) -> [P; 4] {
        let a = &self.nodes[i];
        let b = &self.nodes[(i + 1) % self.nodes.len()];
        [a.p, add(a.p, a.o), add(b.p, b.i), b.p]
    }

    fn is_line(&self, i: usize) -> bool {
        let a = &self.nodes[i];
        let b = &self.nodes[(i + 1) % self.nodes.len()];
        len(a.o) < 1e-4 && len(b.i) < 1e-4
    }

    /// The outline as a polyline.
    fn flatten(&self) -> Vec<P> {
        let mut out = Vec::new();
        if let Some(n) = self.nodes.first() {
            out.push(n.p);
        }
        for i in 0..self.seg_count() {
            let c = self.seg(i);
            if self.is_line(i) {
                out.push(c[3]);
            } else {
                for k in 1..=16 {
                    out.push(bez(&c, k as f32 / 16.0));
                }
            }
        }
        out
    }

    fn d(&self) -> String {
        let mut s = String::new();
        let Some(first) = self.nodes.first() else { return s };
        s += &format!("M{} {}", num(first.p[0]), num(first.p[1]));
        for i in 0..self.seg_count() {
            let c = self.seg(i);
            if self.is_line(i) {
                s += &format!("L{} {}", num(c[3][0]), num(c[3][1]));
            } else {
                s += &format!("C{} {} {} {} {} {}", num(c[1][0]), num(c[1][1]), num(c[2][0]), num(c[2][1]), num(c[3][0]), num(c[3][1]));
            }
        }
        if self.closed {
            s += "Z";
        }
        s
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Geom {
    /// x, y, w, h and the corner radius
    Rect([f32; 4], f32),
    /// centre and radii
    Ellipse(P, P),
    Path(Vec<Sub>),
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Fill {
    None,
    Solid(C),
    /// two colours and the angle of the gradient in degrees
    Linear(C, C, f32),
    Radial(C, C),
    /// spots of colour over a base colour
    Blobs(C, Blobs),
}

impl Fill {
    fn first(&self) -> Option<C> {
        match *self {
            Fill::None => None,
            Fill::Solid(c) | Fill::Linear(c, _, _) | Fill::Radial(c, _) | Fill::Blobs(c, _) => Some(c),
        }
    }
    fn second(&self) -> C {
        match *self {
            Fill::Linear(_, c, _) | Fill::Radial(_, c) => c,
            Fill::Solid(c) | Fill::Blobs(c, _) => c,
            Fill::None => [1.0; 3],
        }
    }
    fn kind(&self) -> usize {
        match self {
            Fill::None => 0,
            Fill::Solid(_) => 1,
            Fill::Linear(..) => 2,
            Fill::Radial(..) => 3,
            Fill::Blobs(..) => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Style {
    pub fill: Fill,
    pub stroke: Option<C>,
    pub width: f32,
    /// 0 butt, 1 round, 2 square
    pub cap: usize,
    /// 0 miter, 1 round, 2 bevel
    pub join: usize,
    pub opacity: f32,
}

impl Default for Style {
    fn default() -> Style {
        Style { fill: Fill::Solid([0.25, 0.52, 0.98]), stroke: None, width: 8.0, cap: 1, join: 1, opacity: 1.0 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Shape {
    pub id: u32,
    pub name: String,
    pub geom: Geom,
    pub style: Style,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Drawing {
    pub shapes: Vec<Shape>,
    pub next: u32,
}

impl Default for Drawing {
    fn default() -> Drawing {
        Drawing { shapes: Vec::new(), next: 1 }
    }
}

// ----------------------------------------------------------------------------------------------------- small maths

fn add(a: P, b: P) -> P {
    [a[0] + b[0], a[1] + b[1]]
}
fn sub(a: P, b: P) -> P {
    [a[0] - b[0], a[1] - b[1]]
}
fn mul(a: P, k: f32) -> P {
    [a[0] * k, a[1] * k]
}
fn len(a: P) -> f32 {
    a[0].hypot(a[1])
}
fn dist(a: P, b: P) -> f32 {
    len(sub(a, b))
}

fn num(v: f32) -> String {
    let s = format!("{:.2}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" || s == "-0" { "0".to_string() } else { s.to_string() }
}

fn bez(c: &[P; 4], t: f32) -> P {
    let u = 1.0 - t;
    let (a, b, d, e) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    [a * c[0][0] + b * c[1][0] + d * c[2][0] + e * c[3][0], a * c[0][1] + b * c[1][1] + d * c[2][1] + e * c[3][1]]
}

/// A bezier cut in two at `t` (de Casteljau).
fn split(c: &[P; 4], t: f32) -> ([P; 4], [P; 4]) {
    let l = |a: P, b: P| [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
    let (ab, bc, cd) = (l(c[0], c[1]), l(c[1], c[2]), l(c[2], c[3]));
    let (abc, bcd) = (l(ab, bc), l(bc, cd));
    let m = l(abc, bcd);
    ([c[0], ab, abc, m], [m, bcd, cd, c[3]])
}

fn dist_to_segment(p: P, a: P, b: P) -> f32 {
    let ab = sub(b, a);
    let l2 = ab[0] * ab[0] + ab[1] * ab[1];
    let t = if l2 < 1e-9 { 0.0 } else { (((p[0] - a[0]) * ab[0] + (p[1] - a[1]) * ab[1]) / l2).clamp(0.0, 1.0) };
    dist(p, add(a, mul(ab, t)))
}

fn inside_polygon(p: P, poly: &[P]) -> bool {
    let mut c = false;
    let mut j = poly.len().wrapping_sub(1);
    for i in 0..poly.len() {
        let (a, b) = (poly[i], poly[j]);
        if (a[1] > p[1]) != (b[1] > p[1]) && p[0] < (b[0] - a[0]) * (p[1] - a[1]) / (b[1] - a[1]) + a[0] {
            c = !c;
        }
        j = i;
    }
    c
}

/// The simplified polyline (Ramer-Douglas-Peucker).
fn simplify(pts: &[P], eps: f32) -> Vec<P> {
    if pts.len() < 3 {
        return pts.to_vec();
    }
    let (a, b) = (pts[0], pts[pts.len() - 1]);
    let (mut worst, mut at) = (0.0, 0);
    for (i, p) in pts.iter().enumerate().take(pts.len() - 1).skip(1) {
        let d = dist_to_segment(*p, a, b);
        if d > worst {
            worst = d;
            at = i;
        }
    }
    if worst > eps {
        let mut left = simplify(&pts[..=at], eps);
        let right = simplify(&pts[at..], eps);
        left.pop();
        left.extend(right);
        left
    } else {
        vec![a, b]
    }
}

/// Affine map x' = a x + c y + e, y' = b x + d y + f.
type M = [f32; 6];

fn apply_m(m: &M, p: P) -> P {
    [m[0] * p[0] + m[2] * p[1] + m[4], m[1] * p[0] + m[3] * p[1] + m[5]]
}
fn apply_lin(m: &M, p: P) -> P {
    [m[0] * p[0] + m[2] * p[1], m[1] * p[0] + m[3] * p[1]]
}
fn translation(d: P) -> M {
    [1.0, 0.0, 0.0, 1.0, d[0], d[1]]
}
fn scaling(about: P, sx: f32, sy: f32) -> M {
    [sx, 0.0, 0.0, sy, about[0] - about[0] * sx, about[1] - about[1] * sy]
}
fn rotation(about: P, a: f32) -> M {
    let (s, c) = a.sin_cos();
    [c, s, -s, c, about[0] - c * about[0] + s * about[1], about[1] - s * about[0] - c * about[1]]
}

// -------------------------------------------------------------------------------------------------- shape methods

const KAPPA: f32 = 0.552_284_75;

impl Geom {
    /// The geometry as paths (a rectangle or ellipse is turned into one).
    fn to_path(&self) -> Vec<Sub> {
        match self {
            Geom::Path(s) => s.clone(),
            Geom::Rect(r, rad) => {
                let [x, y, w, h] = *r;
                let rad = rad.min(w.abs() / 2.0).min(h.abs() / 2.0).max(0.0);
                if rad < 0.01 {
                    let pts = [[x, y], [x + w, y], [x + w, y + h], [x, y + h]];
                    return vec![Sub { nodes: pts.iter().map(|p| Node::corner(*p)).collect(), closed: true }];
                }
                let k = rad * KAPPA;
                let n = |p: P, i: P, o: P| Node { p, i, o, smooth: false };
                let z = [0.0, 0.0];
                // the arcs run from the end of one edge to the start of the next
                let nodes = vec![
                    n([x + rad, y], [-k, 0.0], z),
                    n([x + w - rad, y], z, [k, 0.0]),
                    n([x + w, y + rad], [0.0, -k], z),
                    n([x + w, y + h - rad], z, [0.0, k]),
                    n([x + w - rad, y + h], [k, 0.0], z),
                    n([x + rad, y + h], z, [-k, 0.0]),
                    n([x, y + h - rad], [0.0, k], z),
                    n([x, y + rad], z, [0.0, -k]),
                ];
                vec![Sub { nodes, closed: true }]
            }
            Geom::Ellipse(c, r) => {
                let (kx, ky) = (r[0] * KAPPA, r[1] * KAPPA);
                let n = |p: P, o: P| Node { p, i: mul(o, -1.0), o, smooth: true };
                vec![Sub {
                    nodes: vec![
                        n([c[0], c[1] - r[1]], [kx, 0.0]),
                        n([c[0] + r[0], c[1]], [0.0, ky]),
                        n([c[0], c[1] + r[1]], [-kx, 0.0]),
                        n([c[0] - r[0], c[1]], [0.0, -ky]),
                    ],
                    closed: true,
                }]
            }
        }
    }

    fn polylines(&self) -> Vec<(Vec<P>, bool)> {
        self.to_path().iter().map(|s| (s.flatten(), s.closed)).collect()
    }

    pub fn bbox(&self) -> [f32; 4] {
        match self {
            Geom::Rect(r, _) => [r[0].min(r[0] + r[2]), r[1].min(r[1] + r[3]), r[0].max(r[0] + r[2]), r[1].max(r[1] + r[3])],
            Geom::Ellipse(c, r) => [c[0] - r[0], c[1] - r[1], c[0] + r[0], c[1] + r[1]],
            Geom::Path(_) => {
                let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
                for (pts, _) in self.polylines() {
                    for p in pts {
                        x0 = x0.min(p[0]);
                        y0 = y0.min(p[1]);
                        x1 = x1.max(p[0]);
                        y1 = y1.max(p[1]);
                    }
                }
                if x0 > x1 { [0.0; 4] } else { [x0, y0, x1, y1] }
            }
        }
    }

    pub fn transform(&mut self, m: &M) {
        let axis = m[1].abs() < 1e-6 && m[2].abs() < 1e-6;
        match self {
            Geom::Rect(r, rad) if axis => {
                let (x0, x1) = (m[0] * r[0] + m[4], m[0] * (r[0] + r[2]) + m[4]);
                let (y0, y1) = (m[3] * r[1] + m[5], m[3] * (r[1] + r[3]) + m[5]);
                *r = [x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs()];
                *rad *= m[0].abs().min(m[3].abs());
            }
            Geom::Ellipse(c, r) if axis => {
                *c = apply_m(m, *c);
                *r = [r[0] * m[0].abs(), r[1] * m[3].abs()];
            }
            Geom::Path(subs) => {
                for s in subs {
                    for n in &mut s.nodes {
                        n.p = apply_m(m, n.p);
                        n.i = apply_lin(m, n.i);
                        n.o = apply_lin(m, n.o);
                    }
                }
            }
            _ => {
                let mut subs = self.to_path();
                for s in &mut subs {
                    for n in &mut s.nodes {
                        n.p = apply_m(m, n.p);
                        n.i = apply_lin(m, n.i);
                        n.o = apply_lin(m, n.o);
                    }
                }
                *self = Geom::Path(subs);
            }
        }
    }
}

impl Shape {
    fn hit(&self, p: P, tol: f32) -> bool {
        let lines = self.geom.polylines();
        if self.style.fill != Fill::None {
            let mut crossings = 0;
            for (pts, _) in &lines {
                if inside_polygon(p, pts) {
                    crossings += 1;
                }
            }
            if crossings % 2 == 1 {
                return true;
            }
        }
        let reach = if self.style.stroke.is_some() { (self.style.width / 2.0).max(tol) } else { tol * 0.6 };
        // an unfilled shape is picked on its outline only
        lines.iter().any(|(pts, closed)| {
            let n = pts.len();
            (0..n.saturating_sub(1)).any(|i| dist_to_segment(p, pts[i], pts[i + 1]) <= reach) || (*closed && n > 2 && dist_to_segment(p, pts[n - 1], pts[0]) <= reach)
        })
    }

    fn svg(&self, defs: &mut String) -> String {
        let st = &self.style;
        let mut a = String::new();
        let mut blob_layers = Vec::new();
        match st.fill {
            Fill::None => a += " fill=\"none\"",
            Fill::Solid(c) => a += &format!(" fill=\"{}\"", to_hex(c)),
            Fill::Linear(c1, c2, deg) => {
                let (s, c) = deg.to_radians().sin_cos();
                defs.push_str(&format!(
                    "<linearGradient id=\"g{}\" x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\"><stop offset=\"0\" stop-color=\"{}\"/><stop offset=\"1\" stop-color=\"{}\"/></linearGradient>",
                    self.id,
                    num(0.5 - 0.5 * c),
                    num(0.5 - 0.5 * s),
                    num(0.5 + 0.5 * c),
                    num(0.5 + 0.5 * s),
                    to_hex(c1),
                    to_hex(c2)
                ));
                a += &format!(" fill=\"url(#g{})\"", self.id);
            }
            Fill::Radial(c1, c2) => {
                defs.push_str(&format!("<radialGradient id=\"g{}\"><stop offset=\"0\" stop-color=\"{}\"/><stop offset=\"1\" stop-color=\"{}\"/></radialGradient>", self.id, to_hex(c1), to_hex(c2)));
                a += &format!(" fill=\"url(#g{})\"", self.id);
            }
            Fill::Blobs(base, k) => {
                // the base colour, then a copy of the shape for each blob: a radial gradient from the blob's colour fading out
                a += &format!(" fill=\"{}\"", to_hex(base));
                for (i, b) in k.used().iter().enumerate() {
                    let stops: String = [(0.0, 1.0), (0.25, 0.84), (0.5, 0.5), (0.75, 0.16), (1.0, 0.0)]
                        .iter()
                        .map(|(o, al)| format!("<stop offset=\"{o}\" stop-color=\"{}\" stop-opacity=\"{al}\"/>", to_hex(b.colour)))
                        .collect();
                    defs.push_str(&format!("<radialGradient id=\"g{}b{i}\" cx=\"{}\" cy=\"{}\" r=\"{}\">{stops}</radialGradient>", self.id, num(b.at[0]), num(b.at[1]), num(b.size.max(0.02))));
                    blob_layers.push(format!(" fill=\"url(#g{}b{i})\"", self.id));
                }
            }
        }
        let mut line = String::new();
        if let Some(c) = st.stroke {
            let cap = ["butt", "round", "square"][st.cap.min(2)];
            let join = ["miter", "round", "bevel"][st.join.min(2)];
            line = format!(" stroke=\"{}\" stroke-width=\"{}\" stroke-linecap=\"{cap}\" stroke-linejoin=\"{join}\"", to_hex(c), num(st.width));
        }
        let opacity = if st.opacity < 0.999 { format!(" opacity=\"{}\"", num(st.opacity)) } else { String::new() };
        let el = |attrs: &str| match &self.geom {
            Geom::Rect(r, rad) => format!("<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"{}\"{attrs}/>", num(r[0]), num(r[1]), num(r[2]), num(r[3]), num(*rad)),
            Geom::Ellipse(c, r) => format!("<ellipse cx=\"{}\" cy=\"{}\" rx=\"{}\" ry=\"{}\"{attrs}/>", num(c[0]), num(c[1]), num(r[0]), num(r[1])),
            Geom::Path(subs) => {
                let d: String = subs.iter().map(Sub::d).collect();
                format!("<path d=\"{d}\"{attrs}/>")
            }
        };
        if blob_layers.is_empty() {
            return el(&format!("{a}{line}{opacity}"));
        }
        let mut out = format!("<g{opacity}>{}", el(&a));
        for l in &blob_layers {
            out += &el(l);
        }
        if !line.is_empty() {
            out += &el(&format!(" fill=\"none\"{line}"));
        }
        out + "</g>"
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

impl Drawing {
    pub fn to_svg(&self) -> String {
        let mut defs = String::new();
        let body: String = self.shapes.iter().map(|s| s.svg(&mut defs)).collect();
        let meta = serde_json::to_string(self).unwrap_or_default();
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{s}\" height=\"{s}\" viewBox=\"0 0 {s} {s}\"><metadata id=\"memvec\">{}</metadata><defs>{defs}</defs>{body}</svg>",
            escape(&meta),
            s = SIZE as u32
        )
    }

    /// The drawing in an SVG: our own, or any other turned into paths.
    pub fn from_svg(text: &str) -> Option<Drawing> {
        if text.contains("id=\"memvec\"") {
            let doc = usvg::roxmltree::Document::parse(text).ok()?;
            let m = doc.descendants().find(|n| n.attribute("id") == Some("memvec"))?;
            return serde_json::from_str(m.text()?).ok();
        }
        let tree = usvg::Tree::from_str(text, &usvg::Options::default()).ok()?;
        let sz = tree.size();
        let k = SIZE / sz.width().max(sz.height());
        let (ox, oy) = ((SIZE - sz.width() * k) / 2.0, (SIZE - sz.height() * k) / 2.0);
        let mut d = Drawing::default();
        import_group(tree.root(), k, ox, oy, &mut d);
        Some(d)
    }

    fn fresh(&mut self) -> u32 {
        let id = self.next;
        self.next += 1;
        id
    }

    fn index(&self, id: u32) -> Option<usize> {
        self.shapes.iter().position(|s| s.id == id)
    }

    fn get(&self, id: u32) -> Option<&Shape> {
        self.shapes.iter().find(|s| s.id == id)
    }

    fn get_mut(&mut self, id: u32) -> Option<&mut Shape> {
        self.shapes.iter_mut().find(|s| s.id == id)
    }

    fn push(&mut self, name: &str, geom: Geom, style: Style) -> u32 {
        let id = self.fresh();
        let count = self.shapes.iter().filter(|s| s.name.starts_with(name)).count() + 1;
        self.shapes.push(Shape { id, name: format!("{name} {count}"), geom, style });
        id
    }

    fn bbox_of(&self, ids: &[u32]) -> Option<[f32; 4]> {
        let mut out: Option<[f32; 4]> = None;
        for id in ids {
            if let Some(s) = self.get(*id) {
                let b = s.geom.bbox();
                out = Some(match out {
                    None => b,
                    Some(o) => [o[0].min(b[0]), o[1].min(b[1]), o[2].max(b[2]), o[3].max(b[3])],
                });
            }
        }
        out
    }

    pub fn pick(&self, p: P, tol: f32) -> Option<u32> {
        self.shapes.iter().rev().find(|s| s.hit(p, tol)).map(|s| s.id)
    }
}

fn import_group(g: &usvg::Group, k: f32, ox: f32, oy: f32, out: &mut Drawing) {
    for node in g.children() {
        match node {
            usvg::Node::Group(g2) => import_group(g2, k, ox, oy, out),
            usvg::Node::Path(p) => {
                let t = p.abs_transform();
                let map = |pt: tiny_skia::Point| -> P {
                    let (x, y) = (t.sx * pt.x + t.kx * pt.y + t.tx, t.ky * pt.x + t.sy * pt.y + t.ty);
                    [x * k + ox, y * k + oy]
                };
                let mut subs: Vec<Sub> = Vec::new();
                let mut cur: Option<Sub> = None;
                let finish = |cur: &mut Option<Sub>, subs: &mut Vec<Sub>, closed: bool| {
                    if let Some(mut s) = cur.take() {
                        if closed {
                            s.closed = true;
                            if s.nodes.len() > 1 && dist(s.nodes[0].p, s.nodes[s.nodes.len() - 1].p) < 1e-3 {
                                let last = s.nodes.pop().unwrap();
                                s.nodes[0].i = last.i;
                            }
                        }
                        for n in &mut s.nodes {
                            n.smooth = len(n.i) > 1e-3 && len(n.o) > 1e-3 && (n.i[0] * n.o[1] - n.i[1] * n.o[0]).abs() < 0.05 * len(n.i) * len(n.o);
                        }
                        subs.push(s);
                    }
                };
                for seg in p.data().segments() {
                    match seg {
                        tiny_skia::PathSegment::MoveTo(pt) => {
                            finish(&mut cur, &mut subs, false);
                            cur = Some(Sub { nodes: vec![Node::corner(map(pt))], closed: false });
                        }
                        tiny_skia::PathSegment::LineTo(pt) => {
                            if let Some(s) = cur.as_mut() {
                                s.nodes.push(Node::corner(map(pt)));
                            }
                        }
                        tiny_skia::PathSegment::QuadTo(c, pt) => {
                            if let Some(s) = cur.as_mut() {
                                let p0 = s.nodes[s.nodes.len() - 1].p;
                                let (c, pt) = (map(c), map(pt));
                                let c1 = add(p0, mul(sub(c, p0), 2.0 / 3.0));
                                let c2 = add(pt, mul(sub(c, pt), 2.0 / 3.0));
                                let last = s.nodes.len() - 1;
                                s.nodes[last].o = sub(c1, p0);
                                s.nodes.push(Node { p: pt, i: sub(c2, pt), o: [0.0; 2], smooth: false });
                            }
                        }
                        tiny_skia::PathSegment::CubicTo(c1, c2, pt) => {
                            if let Some(s) = cur.as_mut() {
                                let p0 = s.nodes[s.nodes.len() - 1].p;
                                let (c1, c2, pt) = (map(c1), map(c2), map(pt));
                                let last = s.nodes.len() - 1;
                                s.nodes[last].o = sub(c1, p0);
                                s.nodes.push(Node { p: pt, i: sub(c2, pt), o: [0.0; 2], smooth: false });
                            }
                        }
                        tiny_skia::PathSegment::Close => finish(&mut cur, &mut subs, true),
                    }
                }
                finish(&mut cur, &mut subs, false);
                if subs.is_empty() {
                    continue;
                }
                let colour = |c: usvg::Color| [c.red as f32 / 255.0, c.green as f32 / 255.0, c.blue as f32 / 255.0];
                let paint = |p: &usvg::Paint| -> Fill {
                    match p {
                        usvg::Paint::Color(c) => Fill::Solid(colour(*c)),
                        usvg::Paint::LinearGradient(g) => {
                            let st = g.stops();
                            match (st.first(), st.last()) {
                                (Some(a), Some(b)) => Fill::Linear(colour(a.color()), colour(b.color()), (g.y2() - g.y1()).atan2(g.x2() - g.x1()).to_degrees()),
                                _ => Fill::None,
                            }
                        }
                        usvg::Paint::RadialGradient(g) => {
                            let st = g.stops();
                            match (st.first(), st.last()) {
                                (Some(a), Some(b)) => Fill::Radial(colour(a.color()), colour(b.color())),
                                _ => Fill::None,
                            }
                        }
                        _ => Fill::None,
                    }
                };
                let mut style = Style { fill: Fill::None, stroke: None, ..Style::default() };
                if let Some(f) = p.fill() {
                    style.fill = paint(f.paint());
                    style.opacity = f.opacity().get();
                }
                if let Some(s) = p.stroke() {
                    if let usvg::Paint::Color(c) = s.paint() {
                        style.stroke = Some(colour(*c));
                    } else if let Some(c) = match paint(s.paint()) { Fill::Linear(c, _, _) | Fill::Radial(c, _) | Fill::Solid(c) | Fill::Blobs(c, _) => Some(c), Fill::None => None } {
                        style.stroke = Some(c);
                    }
                    style.width = s.width().get() * k;
                    style.cap = match s.linecap() {
                        usvg::LineCap::Butt => 0,
                        usvg::LineCap::Round => 1,
                        usvg::LineCap::Square => 2,
                    };
                    style.join = match s.linejoin() {
                        usvg::LineJoin::Round => 1,
                        usvg::LineJoin::Bevel => 2,
                        _ => 0,
                    };
                }
                let name = if p.id().is_empty() { "Path".to_string() } else { p.id().to_string() };
                let id = out.fresh();
                out.shapes.push(Shape { id, name, geom: Geom::Path(subs), style });
            }
            _ => {}
        }
    }
}

// ------------------------------------------------------------------------------------------------------ the editor

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tool {
    /// no drawing tool: the owner moves and scales whole layers
    Layer,
    Select,
    Node,
    Rect,
    Ellipse,
    Star,
    Pen,
    Pencil,
    Line,
}

impl Tool {
    pub const ALL: [(Tool, &'static str, &'static str, &'static str); 9] = [
        (Tool::Layer, "mouse-pointer", "Layers (V)", "Click a layer on the icon to pick it, drag to move it, corners scale it. Choose a drawing tool to draw on the icon."),
        (Tool::Select, "mouse-pointer-2", "Shapes (S)", "Click to select, drag to move, corners scale, the round handle rotates. Shift adds to the selection."),
        (Tool::Node, "spline", "Edit nodes (N)", "Drag nodes and handles. Click a segment to add a node. Alt+drag a handle breaks the smooth link."),
        (Tool::Rect, "square", "Rectangle (R)", "Drag to draw. Shift makes a square. The corner radius is in the panel."),
        (Tool::Ellipse, "circle", "Ellipse (E)", "Drag to draw. Shift makes a circle."),
        (Tool::Star, "star", "Star and polygon (Y)", "Drag from the centre. The number of corners and the ratio are in the panel."),
        (Tool::Pen, "pen-tool", "Pen / Bezier (B)", "Click for a corner, drag for a smooth node. Click the first node to close, Enter or double click to finish."),
        (Tool::Pencil, "pencil", "Freehand (P)", "Draw freely: the line is smoothed into a bezier path when you let go."),
        (Tool::Line, "minus", "Line (L)", "Drag to draw a straight line."),
    ];
}

/// Which colour a click on a swatch of the panel asks the colour picker for.
/// A line things line up with: its place, and how far along the other axis the thing it comes from reaches.
#[derive(Clone, Copy)]
struct Tgt {
    pos: f32,
    lo: f32,
    hi: f32,
}

/// Thickness of a ruler.
const RULER: f32 = 16.0;

/// The smallest move that brings one of `feats` onto a line of `ts`, when one is within `reach`.
fn nearest(feats: &[f32], ts: &[Tgt], reach: f32) -> Option<f32> {
    let mut best: Option<f32> = None;
    for f in feats {
        for t in ts {
            let e = t.pos - f;
            if e.abs() < reach && best.map_or(true, |b| e.abs() < b.abs()) {
                best = Some(e);
            }
        }
    }
    best
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Target {
    Fill,
    Fill2,
    Stroke,
    /// the colour of a blob of a blob fill
    Blob(usize),
}

enum Drag {
    Move { from: P, orig: Vec<Shape> },
    Scale { handle: usize, bbox: [f32; 4], orig: Vec<Shape> },
    Rotate { c: P, a0: f32, orig: Vec<Shape> },
    Band { from: P },
    NodeBand { from: P },
    Create { from: P, id: u32, tool: Tool },
    Pencil { pts: Vec<P> },
    Nodes { from: P, orig: Shape },
    Handle { sub: usize, node: usize, out: bool },
    PenNode { id: u32, sub: usize, node: usize, from: P },
}

pub struct Editor {
    pub drawing: Drawing,
    pub tool: Tool,
    pub sel: Vec<u32>,
    /// selected nodes of the first selected shape: (subpath, node)
    nsel: HashSet<(usize, usize)>,
    drag: Option<Drag>,
    /// pixels per canvas unit, and where the canvas origin is relative to the top left of the view
    pub zoom: f32,
    pan: P,
    placed: bool,
    pub cur: Style,
    pub corners: usize,
    pub ratio: f32,
    pub star: bool,
    /// grid size in canvas units (0: no grid) and whether things snap to it and to the canvas
    pub grid: f32,
    pub snap: bool,
    /// snapping to the edges and middles of the other objects, with guide lines shown while it happens
    pub smart: bool,
    /// the rulers along the top and the left of the canvas, and the guides pulled out of them (true: a vertical line at x)
    pub rulers: bool,
    guides: Vec<(bool, f32)>,
    guide_drag: Option<(bool, f32)>,
    clip: Vec<Shape>,
    /// text for the system clipboard (the SVG of what was copied) and a wish to read it, for the owner to carry out
    pub want_copy: Option<String>,
    pub want_paste: bool,
    /// the blob of a blob fill the panel edits
    blob_sel: usize,
    pastes: u32,
    pen: Option<(u32, usize)>,
    /// the drawing changed since the owner last looked
    pub dirty: bool,
    svg: String,
    version: u64,
    over: Option<Arc<Pic>>,
    pub status: String,
    /// the last view rectangle (for fit and zoom buttons)
    view: [f32; 4],
    scroll_ins: f32,
    panel_h: f32,
}

impl Editor {
    pub fn new() -> Editor {
        Editor {
            drawing: Drawing::default(),
            tool: Tool::Layer,
            sel: Vec::new(),
            nsel: HashSet::new(),
            drag: None,
            zoom: 1.0,
            pan: [0.0; 2],
            placed: false,
            cur: Style::default(),
            corners: 5,
            ratio: 0.5,
            star: true,
            grid: 0.0,
            snap: false,
            smart: true,
            rulers: true,
            guides: Vec::new(),
            guide_drag: None,
            clip: Vec::new(),
            want_copy: None,
            want_paste: false,
            blob_sel: 0,
            pastes: 0,
            pen: None,
            dirty: false,
            svg: String::new(),
            version: 1,
            over: None,
            status: String::new(),
            view: [0.0, 0.0, 800.0, 600.0],
            scroll_ins: 0.0,
            panel_h: 0.0,
        }
    }

    /// Start editing `svg` (the SVG of a layer).
    pub fn load(&mut self, svg: &str) {
        let keep = self.sel.clone();
        self.drawing = Drawing::from_svg(svg).unwrap_or_default();
        self.svg = svg.to_string();
        self.sel = keep.into_iter().filter(|id| self.drawing.get(*id).is_some()).collect();
        self.nsel.clear();
        self.drag = None;
        self.pen = None;
        self.version += 1;
    }

    /// Some shapes to look at (`MEMRIUM_ICONS_DEMO=1`), for showing the editor without drawing by hand.
    pub fn demo(&mut self) {
        let grad = Style { fill: Fill::Linear([1.0, 0.62, 0.2], [0.95, 0.25, 0.45], 70.0), ..Style::default() };
        let a = self.drawing.push("Rectangle", Geom::Rect([96.0, 112.0, 320.0, 288.0], 64.0), grad);
        self.drawing.push("Ellipse", Geom::Ellipse([256.0, 250.0], [92.0, 92.0]), Style { fill: Fill::Solid([1.0, 1.0, 1.0]), ..Style::default() });
        let star = star_path([256.0, 250.0], 62.0, -std::f32::consts::FRAC_PI_2, 5, 0.45, true);
        let id = self.drawing.push("Star", Geom::Path(vec![star]), Style { fill: Fill::Solid([0.95, 0.3, 0.45]), stroke: Some([0.6, 0.1, 0.25]), width: 6.0, ..Style::default() });
        let _ = a;
        self.sel = vec![id];
        self.dirty = true;
    }

    /// Open a different layer: the view is fitted again.
    pub fn open(&mut self, svg: &str) {
        self.sel.clear();
        self.load(svg);
        self.placed = false;
    }

    /// The SVG the layer holds now, when it is not the one this editor wrote last.
    pub fn is_current(&self, svg: &str) -> bool {
        self.svg == svg
    }

    /// The SVG of the drawing as it is, remembered as the layer's.
    pub fn commit(&mut self) -> String {
        self.svg = self.drawing.to_svg();
        self.version += 1;
        self.dirty = false;
        self.svg.clone()
    }

    pub fn touch(&mut self) {
        self.dirty = true;
    }

    pub fn is_empty(&self) -> bool {
        self.drawing.shapes.is_empty()
    }

    // ------------------------------------------------------------------------------------------- the view

    fn to_screen(&self, p: P) -> P {
        [self.view[0] + self.pan[0] + p[0] * self.zoom, self.view[1] + self.pan[1] + p[1] * self.zoom]
    }

    fn to_canvas(&self, s: P) -> P {
        [(s[0] - self.view[0] - self.pan[0]) / self.zoom, (s[1] - self.view[1] - self.pan[1]) / self.zoom]
    }

    /// The point on the grid (and the canvas's edges and middle) nearest to `p`.
    fn grid_pt(&self, p: P) -> P {
        let g = if self.grid > 0.0 { self.grid } else { 8.0 };
        let near = |v: f32| -> f32 {
            let mut best = (v / g).round() * g;
            for t in [0.0, SIZE / 2.0, SIZE] {
                if (v - t).abs() * self.zoom < 6.0 {
                    best = t;
                }
            }
            best
        };
        [near(p[0]), near(p[1])]
    }

    /// The lines other things line up with: the canvas's edges and middle, the guides, and the edges and middles of the
    /// objects that are not selected (x lines first, then y lines).
    fn targets(&self) -> (Vec<Tgt>, Vec<Tgt>) {
        let (mut xs, mut ys) = (Vec::new(), Vec::new());
        for t in [0.0, SIZE / 2.0, SIZE] {
            xs.push(Tgt { pos: t, lo: 0.0, hi: SIZE });
            ys.push(Tgt { pos: t, lo: 0.0, hi: SIZE });
        }
        for (vertical, pos) in &self.guides {
            let t = Tgt { pos: *pos, lo: 0.0, hi: SIZE };
            if *vertical { xs.push(t) } else { ys.push(t) }
        }
        for sh in &self.drawing.shapes {
            if self.sel.contains(&sh.id) {
                continue;
            }
            let b = sh.geom.bbox();
            for pos in [b[0], (b[0] + b[2]) / 2.0, b[2]] {
                xs.push(Tgt { pos, lo: b[1], hi: b[3] });
            }
            for pos in [b[1], (b[1] + b[3]) / 2.0, b[3]] {
                ys.push(Tgt { pos, lo: b[0], hi: b[2] });
            }
        }
        (xs, ys)
    }

    fn snap_pt(&self, p: P) -> P {
        let mut out = if self.snap { self.grid_pt(p) } else { p };
        if self.smart {
            let (xs, ys) = self.targets();
            if let Some(e) = nearest(&[p[0]], &xs, 6.0 / self.zoom) {
                out[0] = p[0] + e;
            }
            if let Some(e) = nearest(&[p[1]], &ys, 6.0 / self.zoom) {
                out[1] = p[1] + e;
            }
        }
        out
    }

    /// The guide lines to show while something is moved or drawn: those the moving box (or the snapped point) lines up
    /// with, as (vertical, position, from, to).
    fn active_guides(&self, mouse_c: P) -> Vec<(bool, f32, f32, f32)> {
        if !self.smart {
            return Vec::new();
        }
        let b = match &self.drag {
            Some(Drag::Move { .. }) | Some(Drag::Scale { .. }) => self.selection_box(),
            Some(Drag::Rotate { .. }) | Some(Drag::Band { .. }) | Some(Drag::NodeBand { .. }) | None => None,
            Some(_) => {
                let p = self.snap_pt(mouse_c);
                Some([p[0], p[1], p[0], p[1]])
            }
        };
        let Some(b) = b else { return Vec::new() };
        let (xs, ys) = self.targets();
        let eps = 0.6 / self.zoom;
        let mut out = Vec::new();
        for k in 0..2 {
            let feats = [b[k], (b[k] + b[k + 2]) / 2.0, b[k + 2]];
            for t in if k == 0 { &xs } else { &ys } {
                if feats.iter().any(|f| (f - t.pos).abs() < eps) {
                    let (lo, hi) = (t.lo.min(b[1 - k]), t.hi.max(b[3 - k]));
                    out.push((k == 0, t.pos, lo, hi));
                }
            }
        }
        out
    }

    // --------------------------------------------------------------------------------------------- editing

    fn selected(&self) -> Vec<Shape> {
        self.sel.iter().filter_map(|id| self.drawing.get(*id).cloned()).collect()
    }

    fn selection_box(&self) -> Option<[f32; 4]> {
        self.drawing.bbox_of(&self.sel)
    }

    fn set_style(&mut self, f: impl Fn(&mut Style)) {
        f(&mut self.cur);
        for id in self.sel.clone() {
            if let Some(s) = self.drawing.get_mut(id) {
                f(&mut s.style);
            }
        }
        self.touch();
    }

    pub fn set_colour(&mut self, t: Target, c: C) {
        self.set_style(|s| match t {
            Target::Fill => {
                s.fill = match s.fill {
                    Fill::Linear(_, b, a) => Fill::Linear(c, b, a),
                    Fill::Radial(_, b) => Fill::Radial(c, b),
                    Fill::Blobs(_, k) => Fill::Blobs(c, k),
                    _ => Fill::Solid(c),
                }
            }
            Target::Fill2 => {
                s.fill = match s.fill {
                    Fill::Linear(a, _, ang) => Fill::Linear(a, c, ang),
                    Fill::Radial(a, _) => Fill::Radial(a, c),
                    f => f,
                }
            }
            Target::Blob(i) => {
                if let Fill::Blobs(a, mut k) = s.fill {
                    if i < k.count as usize {
                        k.spots[i].colour = c;
                    }
                    s.fill = Fill::Blobs(a, k);
                }
            }
            Target::Stroke => {
                s.stroke = Some(c);
                if s.width < 0.5 {
                    s.width = 8.0;
                }
            }
        });
    }

    /// The colour of a target of the selection (or of the style new shapes get).
    pub fn colour_of(&self, t: Target) -> C {
        let st = self.selected().first().map(|s| s.style).unwrap_or(self.cur);
        match t {
            Target::Fill => st.fill.first().unwrap_or([0.5; 3]),
            Target::Fill2 => st.fill.second(),
            Target::Stroke => st.stroke.unwrap_or([0.0; 3]),
            Target::Blob(i) => match st.fill {
                Fill::Blobs(_, k) => k.spots.get(i).map_or([0.5; 3], |b| b.colour),
                _ => [0.5; 3],
            },
        }
    }

    fn delete_selected(&mut self) {
        let sel = self.sel.clone();
        self.drawing.shapes.retain(|s| !sel.contains(&s.id));
        self.sel.clear();
        self.nsel.clear();
        self.touch();
    }

    fn duplicate(&mut self) {
        let mut new = Vec::new();
        for s in self.selected() {
            let mut c = s.clone();
            c.id = self.drawing.fresh();
            c.geom.transform(&translation([12.0, 12.0]));
            new.push(c);
        }
        self.sel = new.iter().map(|s| s.id).collect();
        self.drawing.shapes.extend(new);
        self.touch();
    }

    /// Move the selected shapes through the z-order: to the front/back (`all`) or one step.
    pub fn restack(&mut self, up: bool, all: bool) {
        let sel = self.sel.clone();
        if sel.is_empty() {
            return;
        }
        let sh = &mut self.drawing.shapes;
        if all {
            let (mut picked, rest): (Vec<Shape>, Vec<Shape>) = sh.drain(..).partition(|s| sel.contains(&s.id));
            *sh = if up {
                let mut r = rest;
                r.append(&mut picked);
                r
            } else {
                picked.extend(rest);
                picked
            };
        } else if up {
            for i in (0..sh.len().saturating_sub(1)).rev() {
                if sel.contains(&sh[i].id) && !sel.contains(&sh[i + 1].id) {
                    sh.swap(i, i + 1);
                }
            }
        } else {
            for i in 1..sh.len() {
                if sel.contains(&sh[i].id) && !sel.contains(&sh[i - 1].id) {
                    sh.swap(i, i - 1);
                }
            }
        }
        self.touch();
    }

    pub fn transform_selected(&mut self, about_selection: bool, f: impl Fn(P) -> M) {
        let Some(b) = self.selection_box() else { return };
        let c = if about_selection { [(b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0] } else { [SIZE / 2.0, SIZE / 2.0] };
        let m = f(c);
        for id in self.sel.clone() {
            if let Some(s) = self.drawing.get_mut(id) {
                s.geom.transform(&m);
            }
        }
        self.touch();
    }

    pub fn flip(&mut self, horizontal: bool) {
        self.transform_selected(true, |c| if horizontal { scaling(c, -1.0, 1.0) } else { scaling(c, 1.0, -1.0) });
    }

    pub fn rotate_by(&mut self, deg: f32) {
        self.transform_selected(true, |c| rotation(c, deg.to_radians()));
    }

    /// Align the selection to the canvas: 0..2 left, centre, right; 3..5 top, middle, bottom.
    pub fn align(&mut self, how: usize) {
        for id in self.sel.clone() {
            let Some(s) = self.drawing.get(id) else { continue };
            let b = s.geom.bbox();
            let d = match how {
                0 => [-b[0], 0.0],
                1 => [SIZE / 2.0 - (b[0] + b[2]) / 2.0, 0.0],
                2 => [SIZE - b[2], 0.0],
                3 => [0.0, -b[1]],
                4 => [0.0, SIZE / 2.0 - (b[1] + b[3]) / 2.0],
                _ => [0.0, SIZE - b[3]],
            };
            if let Some(s) = self.drawing.get_mut(id) {
                s.geom.transform(&translation(d));
            }
        }
        self.touch();
    }

    /// Set the box of the selection (x, y, w, h) from the panel.
    fn set_box(&mut self, nb: [f32; 4]) {
        let Some(b) = self.selection_box() else { return };
        let (w, h) = ((b[2] - b[0]).max(0.01), (b[3] - b[1]).max(0.01));
        let (sx, sy) = (nb[2].max(0.5) / w, nb[3].max(0.5) / h);
        let m = [sx, 0.0, 0.0, sy, nb[0] - b[0] * sx, nb[1] - b[1] * sy];
        for id in self.sel.clone() {
            if let Some(s) = self.drawing.get_mut(id) {
                s.geom.transform(&m);
            }
        }
        self.touch();
    }

    pub fn to_paths(&mut self) {
        for id in self.sel.clone() {
            if let Some(s) = self.drawing.get_mut(id) {
                if !matches!(s.geom, Geom::Path(_)) {
                    s.geom = Geom::Path(s.geom.to_path());
                }
            }
        }
        self.touch();
    }

    // ------------------------------------------------------------------------------------- node operations

    /// Every selected node, resolved against the first selected shape.
    fn with_nodes(&mut self, f: impl Fn(&mut Sub, usize)) {
        let Some(&id) = self.sel.first() else { return };
        let nsel = self.nsel.clone();
        let Some(Geom::Path(subs)) = self.drawing.get_mut(id).map(|s| &mut s.geom) else { return };
        for (si, ni) in nsel {
            if let Some(sub) = subs.get_mut(si) {
                if ni < sub.nodes.len() {
                    f(sub, ni);
                }
            }
        }
        self.touch();
    }

    pub fn nodes_smooth(&mut self) {
        self.with_nodes(|sub, i| {
            let n = sub.nodes.len();
            let prev = sub.nodes[(i + n - 1) % n].p;
            let next = sub.nodes[(i + 1) % n].p;
            let at = &mut sub.nodes[i];
            if len(at.i) < 1e-3 && len(at.o) < 1e-3 {
                let t = mul(sub_pts(next, prev), 1.0 / 6.0);
                at.o = t;
                at.i = mul(t, -1.0);
            } else {
                // in line: the longer handle decides the direction
                let (src, other) = if len(at.o) >= len(at.i) { (at.o, at.i) } else { (mul(at.i, -1.0), at.o) };
                let dir = mul(src, 1.0 / len(src).max(1e-6));
                at.o = mul(dir, if len(at.o) >= len(at.i) { len(at.o) } else { len(other).max(len(src)) });
                at.i = mul(dir, -if len(at.i) < 1e-3 { len(at.o) } else { len(at.i) });
            }
            at.smooth = true;
        });
    }

    pub fn nodes_cusp(&mut self) {
        self.with_nodes(|sub, i| sub.nodes[i].smooth = false);
    }

    pub fn nodes_straight(&mut self) {
        self.with_nodes(|sub, i| {
            sub.nodes[i].i = [0.0; 2];
            sub.nodes[i].o = [0.0; 2];
            sub.nodes[i].smooth = false;
        });
    }

    /// Cut every segment between two selected nodes in two (or all segments of the shape when fewer than two are selected).
    pub fn nodes_insert(&mut self) {
        let Some(&id) = self.sel.first() else { return };
        let nsel = self.nsel.clone();
        let Some(Geom::Path(subs)) = self.drawing.get_mut(id).map(|s| &mut s.geom) else { return };
        for (si, sub) in subs.iter_mut().enumerate() {
            let all = nsel.len() < 2;
            let mut i = sub.seg_count();
            while i > 0 {
                i -= 1;
                let j = (i + 1) % sub.nodes.len();
                if all || (nsel.contains(&(si, i)) && nsel.contains(&(si, j))) {
                    sub.insert_at(i, 0.5);
                }
            }
        }
        self.nsel.clear();
        self.touch();
    }

    pub fn nodes_delete(&mut self) {
        let Some(&id) = self.sel.first() else { return };
        let nsel = self.nsel.clone();
        let Some(Geom::Path(subs)) = self.drawing.get_mut(id).map(|s| &mut s.geom) else { return };
        for (si, sub) in subs.iter_mut().enumerate() {
            let mut keep = Vec::new();
            for (ni, n) in sub.nodes.iter().enumerate() {
                if !nsel.contains(&(si, ni)) {
                    keep.push(*n);
                }
            }
            sub.nodes = keep;
        }
        subs.retain(|s| s.nodes.len() >= 2);
        let empty = subs.is_empty();
        self.nsel.clear();
        if empty {
            self.delete_selected();
        }
        self.touch();
    }

    /// Close an open subpath that holds a selected node, or open a closed one.
    pub fn nodes_toggle_closed(&mut self) {
        let Some(&id) = self.sel.first() else { return };
        let nsel = self.nsel.clone();
        let Some(Geom::Path(subs)) = self.drawing.get_mut(id).map(|s| &mut s.geom) else { return };
        for (si, sub) in subs.iter_mut().enumerate() {
            if nsel.is_empty() || nsel.iter().any(|(s, _)| *s == si) {
                if sub.nodes.len() > 2 {
                    sub.closed = !sub.closed;
                }
            }
        }
        self.touch();
    }

    // ------------------------------------------------------------------------------- clipboard and menus

    /// The SVG of the selected shapes, for the clipboard.
    fn selection_svg(&self) -> String {
        let shapes = self.selected();
        Drawing { shapes, next: self.drawing.next }.to_svg()
    }

    pub fn copy(&mut self) {
        if self.sel.is_empty() {
            return;
        }
        self.clip = self.selected();
        self.want_copy = Some(self.selection_svg());
        self.pastes = 0;
    }

    pub fn cut(&mut self) {
        if self.sel.is_empty() {
            return;
        }
        self.copy();
        self.delete_selected();
    }

    fn add_pasted(&mut self, shapes: Vec<Shape>) -> bool {
        if shapes.is_empty() {
            return false;
        }
        self.pastes += 1;
        let d = 12.0 * self.pastes as f32;
        let mut new = Vec::new();
        for mut s in shapes {
            s.id = self.drawing.fresh();
            s.geom.transform(&translation([d, d]));
            new.push(s);
        }
        self.sel = new.iter().map(|s| s.id).collect();
        self.nsel.clear();
        self.drawing.shapes.extend(new);
        self.touch();
        true
    }

    /// Text from the system clipboard: an SVG (copied here or from elsewhere) becomes shapes.
    pub fn paste_svg(&mut self, text: &str) -> bool {
        if !text.contains("<svg") {
            return false;
        }
        match Drawing::from_svg(text) {
            Some(d) => self.add_pasted(d.shapes),
            None => false,
        }
    }

    /// What was copied inside the editor, when the system clipboard holds no picture.
    pub fn paste_internal(&mut self) -> bool {
        let clip = self.clip.clone();
        self.add_pasted(clip)
    }

    /// Run `f` on the subpaths the effects work on: the ones with selected nodes in the node tool, else every subpath of
    /// every selected shape (turned into paths). `f` gets the selected node numbers (empty: all).
    fn on_subs(&mut self, f: &dyn Fn(&mut Sub, &HashSet<usize>)) {
        let node_mode = self.tool == Tool::Node && !self.nsel.is_empty();
        let ids: Vec<u32> = if node_mode { self.sel.iter().take(1).copied().collect() } else { self.sel.clone() };
        let nsel = self.nsel.clone();
        for id in ids {
            let Some(s) = self.drawing.get_mut(id) else { continue };
            if !matches!(s.geom, Geom::Path(_)) {
                s.geom = Geom::Path(s.geom.to_path());
            }
            if let Geom::Path(subs) = &mut s.geom {
                for (si, sub) in subs.iter_mut().enumerate() {
                    let set: HashSet<usize> = if node_mode { nsel.iter().filter(|(a, _)| *a == si).map(|(_, b)| *b).collect() } else { HashSet::new() };
                    if node_mode && set.is_empty() {
                        continue;
                    }
                    f(sub, &set);
                }
            }
        }
        self.nsel.clear();
        self.touch();
    }

    pub fn contains(&self, screen: P) -> bool {
        inside(screen, self.view)
    }

    /// The menu for a right click at `at` (window pixels). The thing under the pointer is selected first.
    pub fn context_items(&mut self, at: P) -> Vec<Item> {
        let m = self.to_canvas(at);
        if self.tool == Tool::Node {
            if let Some(Geom::Path(subs)) = self.sel.first().and_then(|id| self.drawing.get(*id)).map(|s| s.geom.clone()) {
                for (si, sub) in subs.iter().enumerate() {
                    for (ni, n) in sub.nodes.iter().enumerate() {
                        if dist(self.to_screen(n.p), at) < 9.0 && !self.nsel.contains(&(si, ni)) {
                            self.nsel.clear();
                            self.nsel.insert((si, ni));
                        }
                    }
                }
            }
        }
        if let Some(id) = self.drawing.pick(m, 6.0 / self.zoom) {
            if !self.sel.contains(&id) {
                self.sel = vec![id];
                self.nsel.clear();
                if self.tool == Tool::Node {
                    self.edit_path();
                }
            }
        }
        let has = !self.sel.is_empty();
        let paste_ok = true;
        let effects = Item::parent("Effects", vec![Item::new(1020, "Round corners"), Item::new(1022, "Roughen"), Item::new(1021, "Reverse direction")]);
        let edit = |has: bool| -> Vec<Item> {
            vec![
                Item::new(1054, "Cut").key("Ctrl+X").enabled(has),
                Item::new(1053, "Copy").key("Ctrl+C").enabled(has),
                Item::new(1050, "Paste").key("Ctrl+V").enabled(paste_ok),
                Item::new(1030, "Duplicate").key("Ctrl+D").enabled(has),
                Item::new(1031, "Delete").key("Del").enabled(has),
            ]
        };
        let mut out: Vec<Item> = Vec::new();
        if self.tool == Tool::Node && has && !self.nsel.is_empty() {
            out.push(Item::new(1001, "Smooth node"));
            out.push(Item::new(1002, "Corner node"));
            out.push(Item::new(1003, "Make straight"));
            out.push(Item::new(1004, "Insert node"));
            out.push(Item::new(1005, "Delete node"));
            out.push(Item::new(1006, "Open / close path"));
            out.push(Item::sep());
            out.push(Item::new(1010, "Simplify"));
            out.push(effects);
            out.push(Item::sep());
            out.extend(edit(has));
        } else if has {
            if self.tool == Tool::Node {
                out.push(Item::new(1004, "Insert nodes"));
                out.push(Item::new(1006, "Open / close path"));
            } else {
                out.push(Item::new(1040, "Edit nodes").key("N"));
            }
            out.push(Item::new(1010, "Simplify"));
            out.push(effects);
            out.push(Item::sep());
            out.extend(edit(has));
            out.push(Item::sep());
            out.push(Item::new(1032, "Bring to front").key("Home"));
            out.push(Item::new(1034, "Raise").key("Page Up"));
            out.push(Item::new(1035, "Lower").key("Page Down"));
            out.push(Item::new(1033, "Send to back").key("End"));
            out.push(Item::sep());
            out.push(Item::new(1036, "Flip horizontally"));
            out.push(Item::new(1037, "Flip vertically"));
            out.push(Item::new(1038, "Turn right"));
            out.push(Item::new(1039, "Object to path"));
        } else {
            out.push(Item::new(1050, "Paste").key("Ctrl+V").enabled(paste_ok));
            out.push(Item::new(1051, "Select all").key("Ctrl+A"));
        }
        out
    }

    /// An entry of the context menu or of the program's Edit menu.
    pub fn menu_action(&mut self, id: u32) {
        match id {
            1001 => self.nodes_smooth(),
            1002 => self.nodes_cusp(),
            1003 => self.nodes_straight(),
            1004 => self.nodes_insert(),
            1005 => self.nodes_delete(),
            1006 => self.nodes_toggle_closed(),
            1010 => self.on_subs(&|sub, _| *sub = sub.simplified()),
            1020 => self.on_subs(&|sub, which| sub.round_corners(which, 24.0)),
            1021 => self.on_subs(&|sub, _| sub.reverse()),
            1022 => self.on_subs(&|sub, which| sub.roughen(which, 6.0)),
            1030 => self.duplicate(),
            1031 => self.delete_selected(),
            1032 => self.restack(true, true),
            1033 => self.restack(false, true),
            1034 => self.restack(true, false),
            1035 => self.restack(false, false),
            1036 => self.flip(true),
            1037 => self.flip(false),
            1038 => self.rotate_by(90.0),
            1039 => self.to_paths(),
            1040 => self.set_tool(Tool::Node),
            1050 => self.want_paste = true,
            1051 => self.sel = self.drawing.shapes.iter().map(|s| s.id).collect(),
            1053 => self.copy(),
            1054 => self.cut(),
            _ => {}
        }
    }

    /// The path to edit: the first selected shape, turned into a path.
    fn edit_path(&mut self) -> bool {
        let Some(&id) = self.sel.first() else { return false };
        if let Some(s) = self.drawing.get_mut(id) {
            if !matches!(s.geom, Geom::Path(_)) {
                s.geom = Geom::Path(s.geom.to_path());
                self.touch();
            }
            return true;
        }
        false
    }

    // -------------------------------------------------------------------------------------------- keyboard

    /// A key. `letter` is the key as a letter whatever the layout. True when it was used.
    pub fn key(&mut self, k: Keysym, letter: Option<char>, mods: &Modifiers) -> bool {
        if mods.ctrl {
            match letter {
                Some('a') => {
                    self.sel = self.drawing.shapes.iter().map(|s| s.id).collect();
                    return true;
                }
                Some('d') => {
                    self.duplicate();
                    return true;
                }
                Some('c') => {
                    self.copy();
                    return true;
                }
                Some('x') => {
                    self.cut();
                    return true;
                }
                Some('v') => {
                    self.want_paste = true;
                    return true;
                }
                _ => return false,
            }
        }
        let step = if mods.shift { 10.0 } else { 1.0 };
        match k {
            Keysym::Escape => {
                if self.pen.take().is_some() {
                    self.touch();
                } else if !self.sel.is_empty() {
                    self.sel.clear();
                    self.nsel.clear();
                } else {
                    return false;
                }
                return true;
            }
            Keysym::Return | Keysym::KP_Enter if self.pen.is_some() => {
                self.pen = None;
                self.touch();
                return true;
            }
            Keysym::Delete | Keysym::BackSpace => {
                if self.tool == Tool::Node && !self.nsel.is_empty() {
                    self.nodes_delete();
                } else if !self.sel.is_empty() {
                    self.delete_selected();
                } else {
                    return false;
                }
                return true;
            }
            Keysym::Left | Keysym::Right | Keysym::Up | Keysym::Down if !self.sel.is_empty() => {
                let d = match k {
                    Keysym::Left => [-step, 0.0],
                    Keysym::Right => [step, 0.0],
                    Keysym::Up => [0.0, -step],
                    _ => [0.0, step],
                };
                if self.tool == Tool::Node && !self.nsel.is_empty() {
                    self.with_nodes(|sub, i| sub.nodes[i].p = add(sub.nodes[i].p, d));
                } else {
                    for id in self.sel.clone() {
                        if let Some(s) = self.drawing.get_mut(id) {
                            s.geom.transform(&translation(d));
                        }
                    }
                    self.touch();
                }
                return true;
            }
            Keysym::Home => self.restack(true, true),
            Keysym::End => self.restack(false, true),
            Keysym::Page_Up => self.restack(true, false),
            Keysym::Page_Down => self.restack(false, false),
            _ => {
                let tool = match letter {
                    Some('v') => Tool::Layer,
                    Some('s') => Tool::Select,
                    Some('n') => Tool::Node,
                    Some('r') => Tool::Rect,
                    Some('e') => Tool::Ellipse,
                    Some('y') => Tool::Star,
                    Some('b') => Tool::Pen,
                    Some('p') => Tool::Pencil,
                    Some('l') => Tool::Line,
                    _ => return false,
                };
                self.set_tool(tool);
                return true;
            }
        }
        true
    }

    pub fn set_tool(&mut self, t: Tool) {
        if self.tool != t {
            if self.pen.take().is_some() {
                self.touch();
            }
            self.drag = None;
            self.nsel.clear();
            self.tool = t;
            if t == Tool::Node {
                self.edit_path();
            }
        }
    }

    // ------------------------------------------------------------------------------------------ the frame

    /// The path tools draw lines: stroke, no fill.
    fn line_style(&self) -> Style {
        let c = self.cur.stroke.or_else(|| self.cur.fill.first()).unwrap_or([0.1; 3]);
        Style { fill: Fill::None, stroke: Some(c), width: self.cur.width.max(2.0), ..self.cur }
    }

    /// Run the editor over the icon for one frame: the canvas is the square of `side` window pixels centred on `centre`
    /// (where the layer is drawn on the icon), and `area` is where the pointer is taken. The picture of the icon is the
    /// owner's; the editor draws only its outlines, nodes and handles over it. True when the drawing changed.
    pub fn frame(&mut self, ui: &mut Ui<'_>, area: [f32; 4], scale: f32, centre: P, side: f32) -> bool {
        self.view = area;
        self.zoom = (side / SIZE).max(0.01);
        self.pan = [centre[0] - side / 2.0 - area[0], centre[1] - side / 2.0 - area[1]];
        let over_view = inside(ui.mouse, area);
        let m = self.to_canvas(ui.mouse);
        let tol = 6.0 / self.zoom;
        let before = self.drawing.clone();
        let on_ruler = self.on_ruler(ui.mouse);
        let grabbed = self.guide_step(ui, on_ruler);
        let press = ui.click && over_view && !on_ruler && !grabbed;

        // the drag in progress, or a new press
        if self.drag.is_some() {
            if ui.held {
                self.drag_step(ui, m, tol);
            } else {
                self.drag_end(ui, m);
            }
        } else if press {
            self.press(ui, m, tol);
        } else if over_view && !on_ruler && self.guide_drag.is_none() {
            self.hover_cursor(ui, m, tol);
        }
        if ui.double && over_view && self.tool == Tool::Pen && self.pen.is_some() {
            if let Some((id, si)) = self.pen.take() {
                if let Some(Geom::Path(subs)) = self.drawing.get_mut(id).map(|s| &mut s.geom) {
                    if let Some(sub) = subs.get_mut(si) {
                        if sub.nodes.len() > 2 && dist(sub.nodes[sub.nodes.len() - 1].p, sub.nodes[sub.nodes.len() - 2].p) < 2.0 {
                            sub.nodes.pop();
                        }
                    }
                }
            }
        }
        let changed = self.drawing != before;
        if changed {
            self.dirty = true;
            self.version += 1;
        }
        self.status = self.status_text();
        self.paint(ui, area, scale);
        changed
    }

    /// The rulers sit just outside the canvas: along its top (with the corner) and along its left side.
    fn ruler_rects(&self) -> ([f32; 4], [f32; 4]) {
        let (a, b) = (self.to_screen([0.0, 0.0]), self.to_screen([SIZE, SIZE]));
        ([a[0] - RULER, a[1] - RULER, b[0] - a[0] + RULER, RULER], [a[0] - RULER, a[1], RULER, b[1] - a[1]])
    }

    fn on_ruler(&self, p: P) -> bool {
        let (top, left) = self.ruler_rects();
        self.rulers && (inside(p, top) || inside(p, left))
    }

    /// Pulling guides out of the rulers, moving them and taking them back. True when the press was used.
    fn guide_step(&mut self, ui: &mut Ui<'_>, on_ruler: bool) -> bool {
        let m = self.to_canvas(ui.mouse);
        if let Some((vertical, _)) = self.guide_drag {
            ui.want_cursor(if vertical { Cursor::ResizeH } else { Cursor::ResizeV });
            if ui.held {
                self.guide_drag = Some((vertical, if vertical { m[0] } else { m[1] }));
            } else {
                // let go over the canvas to keep it, over a ruler to throw it away
                if inside(ui.mouse, self.view) && !on_ruler {
                    self.guides.push((vertical, if vertical { m[0] } else { m[1] }));
                }
                self.guide_drag = None;
            }
            return true;
        }
        if self.drag.is_some() {
            return false;
        }
        if on_ruler {
            let (tr, lr) = self.ruler_rects();
            let (top, left) = (inside(ui.mouse, tr) && ui.mouse[0] >= tr[0] + RULER, inside(ui.mouse, lr));
            if top != left {
                ui.want_cursor(if top { Cursor::ResizeV } else { Cursor::ResizeH });
                if ui.click {
                    self.guide_drag = Some((left, if left { m[0] } else { m[1] }));
                    return true;
                }
            }
            return false;
        }
        if self.tool == Tool::Select && inside(ui.mouse, self.view) {
            let reach = 4.0 / self.zoom;
            if let Some(i) = self.guides.iter().position(|(v, pos)| (pos - if *v { m[0] } else { m[1] }).abs() < reach) {
                let vertical = self.guides[i].0;
                ui.want_cursor(if vertical { Cursor::ResizeH } else { Cursor::ResizeV });
                if ui.click {
                    self.guides.remove(i);
                    self.guide_drag = Some((vertical, if vertical { m[0] } else { m[1] }));
                    return true;
                }
            }
        }
        false
    }

    /// What the tool in hand does, in a line.
    pub fn hint(&self) -> String {
        self.status_text()
    }

    fn status_text(&self) -> String {
        let t = Tool::ALL.iter().find(|t| t.0 == self.tool).map(|t| t.3).unwrap_or("");
        t.to_string()
    }

    fn handles(&self) -> Option<([f32; 4], Vec<P>, P)> {
        let b = self.selection_box()?;
        let (x0, y0, x1, y1) = (b[0], b[1], b[2], b[3]);
        let (mx, my) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
        let pts = vec![[x0, y0], [mx, y0], [x1, y0], [x1, my], [x1, y1], [mx, y1], [x0, y1], [x0, my]];
        let rot = [mx, y0 - 26.0 / self.zoom];
        Some((b, pts, rot))
    }

    fn hover_cursor(&self, ui: &mut Ui<'_>, m: P, tol: f32) {
        match self.tool {
            Tool::Layer => {}
            Tool::Select => {
                if let Some((_, pts, rot)) = self.handles() {
                    let near = |p: P| dist(self.to_screen(p), ui.mouse) < 9.0;
                    if near(rot) {
                        ui.want_cursor(Cursor::Grab);
                        return;
                    }
                    for (i, p) in pts.iter().enumerate() {
                        if near(*p) {
                            ui.want_cursor(if i % 4 == 0 { if i == 0 || i == 4 { Cursor::ResizeNwse } else { Cursor::ResizeNesw } } else if i % 2 == 1 { Cursor::ResizeV } else { Cursor::ResizeH });
                            return;
                        }
                    }
                }
                if self.drawing.pick(m, tol).is_some() {
                    ui.want_cursor(Cursor::Move);
                }
            }
            Tool::Node => ui.want_cursor(Cursor::Pointer),
            _ => ui.want_cursor(Cursor::Crosshair),
        }
    }

    fn press(&mut self, ui: &mut Ui<'_>, m: P, tol: f32) {
        let shift = ui.mods.shift;
        match self.tool {
            Tool::Layer => {}
            Tool::Select => {
                if let Some((b, pts, rot)) = self.handles() {
                    if dist(self.to_screen(rot), ui.mouse) < 9.0 {
                        let c = [(b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0];
                        self.drag = Some(Drag::Rotate { c, a0: (m[1] - c[1]).atan2(m[0] - c[0]), orig: self.selected() });
                        return;
                    }
                    for (i, p) in pts.iter().enumerate() {
                        if dist(self.to_screen(*p), ui.mouse) < 9.0 {
                            self.drag = Some(Drag::Scale { handle: i, bbox: b, orig: self.selected() });
                            return;
                        }
                    }
                }
                match self.drawing.pick(m, tol) {
                    Some(id) => {
                        if shift {
                            if let Some(at) = self.sel.iter().position(|s| *s == id) {
                                self.sel.remove(at);
                                return;
                            }
                            self.sel.push(id);
                        } else if !self.sel.contains(&id) {
                            self.sel = vec![id];
                        }
                        self.drag = Some(Drag::Move { from: m, orig: self.selected() });
                    }
                    None => {
                        if !shift {
                            self.sel.clear();
                        }
                        self.drag = Some(Drag::Band { from: m });
                    }
                }
            }
            Tool::Node => self.press_node(ui, m, tol),
            Tool::Rect | Tool::Ellipse | Tool::Star | Tool::Line => {
                let p = self.snap_pt(m);
                let style = if self.tool == Tool::Line { self.line_style() } else { self.cur };
                let (name, geom) = match self.tool {
                    Tool::Rect => ("Rectangle", Geom::Rect([p[0], p[1], 0.0, 0.0], 0.0)),
                    Tool::Ellipse => ("Ellipse", Geom::Ellipse(p, [0.0, 0.0])),
                    Tool::Star => ("Star", Geom::Path(Vec::new())),
                    _ => ("Line", Geom::Path(vec![Sub { nodes: vec![Node::corner(p), Node::corner(p)], closed: false }])),
                };
                let id = self.drawing.push(name, geom, style);
                self.sel = vec![id];
                self.drag = Some(Drag::Create { from: p, id, tool: self.tool });
            }
            Tool::Pencil => self.drag = Some(Drag::Pencil { pts: vec![m] }),
            Tool::Pen => {
                let p = self.snap_pt(m);
                match self.pen {
                    None => {
                        let id = self.drawing.push("Path", Geom::Path(vec![Sub { nodes: vec![Node::corner(p)], closed: false }]), self.line_style());
                        self.sel = vec![id];
                        self.pen = Some((id, 0));
                        self.drag = Some(Drag::PenNode { id, sub: 0, node: 0, from: p });
                    }
                    Some((id, si)) => {
                        let fill = self.cur.fill;
                        let (view, pan, zoom) = (self.view, self.pan, self.zoom);
                        let Some(Geom::Path(subs)) = self.drawing.get_mut(id).map(|s| &mut s.geom) else {
                            self.pen = None;
                            return;
                        };
                        let sub = &mut subs[si];
                        let first = sub.nodes[0].p;
                        let near_first = sub.nodes.len() > 2 && dist(to_screen_at(view, pan, zoom, first), ui.mouse) < 9.0;
                        if near_first {
                            sub.closed = true;
                            if let Some(s) = self.drawing.get_mut(id) {
                                s.style.fill = if fill == Fill::None { Fill::None } else { fill };
                            }
                            self.pen = None;
                            return;
                        }
                        sub.nodes.push(Node::corner(p));
                        let node = sub.nodes.len() - 1;
                        self.drag = Some(Drag::PenNode { id, sub: si, node, from: p });
                    }
                }
            }
        }
    }

    fn press_node(&mut self, ui: &mut Ui<'_>, m: P, tol: f32) {
        let shift = ui.mods.shift;
        let screen_tol = 8.0;
        // the selected shape's handles, nodes, then a segment
        if let (Some(&id), true) = (self.sel.first(), !self.sel.is_empty()) {
            if let Some(Geom::Path(subs)) = self.drawing.get(id).map(|s| s.geom.clone()) {
                for (si, sub) in subs.iter().enumerate() {
                    for (ni, n) in sub.nodes.iter().enumerate() {
                        if self.nsel.contains(&(si, ni)) {
                            for out in [false, true] {
                                let h = add(n.p, if out { n.o } else { n.i });
                                if len(if out { n.o } else { n.i }) > 1e-3 && dist(self.to_screen(h), ui.mouse) < screen_tol {
                                    self.drag = Some(Drag::Handle { sub: si, node: ni, out });
                                    return;
                                }
                            }
                        }
                    }
                }
                for (si, sub) in subs.iter().enumerate() {
                    for (ni, n) in sub.nodes.iter().enumerate() {
                        if dist(self.to_screen(n.p), ui.mouse) < screen_tol {
                            if shift {
                                if !self.nsel.remove(&(si, ni)) {
                                    self.nsel.insert((si, ni));
                                }
                            } else if !self.nsel.contains(&(si, ni)) {
                                self.nsel.clear();
                                self.nsel.insert((si, ni));
                            }
                            if let Some(s) = self.drawing.get(id) {
                                self.drag = Some(Drag::Nodes { from: m, orig: s.clone() });
                            }
                            return;
                        }
                    }
                }
                // a click on a segment adds a node there
                for (si, sub) in subs.iter().enumerate() {
                    for i in 0..sub.seg_count() {
                        let c = sub.seg(i);
                        let (mut best, mut bt) = (f32::MAX, 0.0);
                        for k in 0..=40 {
                            let t = k as f32 / 40.0;
                            let d = dist(bez(&c, t), m);
                            if d < best {
                                best = d;
                                bt = t;
                            }
                        }
                        if best < tol && bt > 0.02 && bt < 0.98 {
                            if let Some(Geom::Path(subs)) = self.drawing.get_mut(id).map(|s| &mut s.geom) {
                                subs[si].insert_at(i, bt);
                                self.nsel.clear();
                                self.nsel.insert((si, i + 1));
                                let orig = self.drawing.get(id).cloned();
                                if let Some(o) = orig {
                                    self.drag = Some(Drag::Nodes { from: m, orig: o });
                                }
                            }
                            return;
                        }
                    }
                }
            }
        }
        match self.drawing.pick(m, tol) {
            Some(id) => {
                if self.sel != vec![id] {
                    self.sel = vec![id];
                    self.nsel.clear();
                }
                self.edit_path();
            }
            None => {
                self.nsel.clear();
                self.drag = Some(Drag::NodeBand { from: m });
                if !shift {
                    // an empty click keeps the shape selected but drops the node selection
                }
            }
        }
    }

    fn drag_step(&mut self, ui: &mut Ui<'_>, m: P, _tol: f32) {
        let mods = ui.mods;
        let Some(drag) = self.drag.take() else { return };
        match &drag {
            Drag::Move { from, orig } => {
                let mut d = sub(m, *from);
                // a straight move with Ctrl
                if mods.ctrl {
                    if d[0].abs() > d[1].abs() { d[1] = 0.0 } else { d[0] = 0.0 }
                }
                if self.snap || self.smart {
                    let b = bbox_union(orig);
                    let (txs, tys) = if self.smart { self.targets() } else { (Vec::new(), Vec::new()) };
                    let reach = 6.0 / self.zoom;
                    for k in 0..2 {
                        let feats = [b[k] + d[k], (b[k] + b[k + 2]) / 2.0 + d[k], b[k + 2] + d[k]];
                        let mut best = nearest(&feats, if k == 0 { &txs } else { &tys }, reach);
                        if self.snap {
                            for f in feats {
                                let mut at = [0.0; 2];
                                at[k] = f;
                                let e = self.grid_pt(at)[k] - f;
                                if e.abs() < reach && best.map_or(true, |v| e.abs() < v.abs()) {
                                    best = Some(e);
                                }
                            }
                        }
                        if let Some(e) = best {
                            d[k] += e;
                        }
                    }
                }
                for o in orig {
                    if let Some(s) = self.drawing.get_mut(o.id) {
                        s.geom = o.geom.clone();
                        s.geom.transform(&translation(d));
                    }
                }
                ui.want_cursor(Cursor::Move);
            }
            Drag::Scale { handle, bbox, orig } => {
                let pts = [[bbox[0], bbox[1]], [(bbox[0] + bbox[2]) / 2.0, bbox[1]], [bbox[2], bbox[1]], [bbox[2], (bbox[1] + bbox[3]) / 2.0], [bbox[2], bbox[3]], [(bbox[0] + bbox[2]) / 2.0, bbox[3]], [bbox[0], bbox[3]], [bbox[0], (bbox[1] + bbox[3]) / 2.0]];
                let (h, anchor) = (pts[*handle], pts[(*handle + 4) % 8]);
                let mp = self.snap_pt(m);
                let mut sx = if (h[0] - anchor[0]).abs() < 1e-3 { 1.0 } else { (mp[0] - anchor[0]) / (h[0] - anchor[0]) };
                let mut sy = if (h[1] - anchor[1]).abs() < 1e-3 { 1.0 } else { (mp[1] - anchor[1]) / (h[1] - anchor[1]) };
                if mods.shift && handle % 2 == 0 {
                    let k = if sx.abs() > sy.abs() { sx } else { sy };
                    sx = k;
                    sy = k;
                }
                let mm = scaling(anchor, sx, sy);
                for o in orig {
                    if let Some(s) = self.drawing.get_mut(o.id) {
                        s.geom = o.geom.clone();
                        s.geom.transform(&mm);
                    }
                }
                ui.want_cursor(Cursor::ResizeNwse);
            }
            Drag::Rotate { c, a0, orig } => {
                let mut a = (m[1] - c[1]).atan2(m[0] - c[0]) - a0;
                if mods.shift {
                    let step = 15f32.to_radians();
                    a = (a / step).round() * step;
                }
                let mm = rotation(*c, a);
                for o in orig {
                    if let Some(s) = self.drawing.get_mut(o.id) {
                        s.geom = o.geom.clone();
                        s.geom.transform(&mm);
                    }
                }
                ui.want_cursor(Cursor::Grabbing);
            }
            Drag::Create { from, id, tool } => {
                let p = self.snap_pt(m);
                let (id, tool, from) = (*id, *tool, *from);
                let mut q = p;
                if mods.shift && matches!(tool, Tool::Rect | Tool::Ellipse) {
                    let d = (p[0] - from[0]).abs().max((p[1] - from[1]).abs());
                    q = [from[0] + d * (p[0] - from[0]).signum(), from[1] + d * (p[1] - from[1]).signum()];
                }
                let (corners, ratio, star) = (self.corners, self.ratio, self.star);
                if let Some(s) = self.drawing.get_mut(id) {
                    match tool {
                        Tool::Rect => {
                            let rad = if let Geom::Rect(_, r) = s.geom { r } else { 0.0 };
                            s.geom = Geom::Rect([from[0].min(q[0]), from[1].min(q[1]), (q[0] - from[0]).abs(), (q[1] - from[1]).abs()], rad);
                        }
                        Tool::Ellipse => s.geom = Geom::Ellipse([(from[0] + q[0]) / 2.0, (from[1] + q[1]) / 2.0], [(q[0] - from[0]).abs() / 2.0, (q[1] - from[1]).abs() / 2.0]),
                        Tool::Star => {
                            let r = dist(from, q);
                            let a = (q[1] - from[1]).atan2(q[0] - from[0]);
                            s.geom = Geom::Path(vec![star_path(from, r, a, corners, ratio, star)]);
                        }
                        _ => {
                            if let Geom::Path(subs) = &mut s.geom {
                                let mut e = q;
                                if mods.shift {
                                    let d = sub(q, from);
                                    let ang = d[1].atan2(d[0]);
                                    let step = 15f32.to_radians();
                                    let a = (ang / step).round() * step;
                                    e = add(from, mul([a.cos(), a.sin()], len(d)));
                                }
                                subs[0].nodes[1] = Node::corner(e);
                            }
                        }
                    }
                }
            }
            Drag::Pencil { pts } => {
                let mut pts = pts.clone();
                if pts.last().map_or(true, |l| dist(*l, m) > 1.0 / self.zoom) {
                    pts.push(m);
                }
                self.drag = Some(Drag::Pencil { pts });
                return;
            }
            Drag::Nodes { from, orig } => {
                let d = sub(m, *from);
                let nsel = self.nsel.clone();
                if let Some(s) = self.drawing.get_mut(orig.id) {
                    s.geom = orig.geom.clone();
                    if let Geom::Path(subs) = &mut s.geom {
                        for (si, ni) in nsel {
                            if let Some(n) = subs.get_mut(si).and_then(|s| s.nodes.get_mut(ni)) {
                                n.p = add(n.p, d);
                            }
                        }
                    }
                }
                ui.want_cursor(Cursor::Move);
            }
            Drag::Handle { sub: si, node, out } => {
                let id = self.sel.first().copied();
                let p = self.snap_pt(m);
                if let Some(Geom::Path(subs)) = id.and_then(|id| self.drawing.get_mut(id)).map(|s| &mut s.geom) {
                    if let Some(n) = subs.get_mut(*si).and_then(|s| s.nodes.get_mut(*node)) {
                        let h = sub(p, n.p);
                        if mods.alt {
                            n.smooth = false;
                        }
                        if *out {
                            n.o = h;
                        } else {
                            n.i = h;
                        }
                        if n.smooth {
                            let other = len(if *out { n.i } else { n.o }).max(1e-3);
                            let dir = mul(h, -1.0 / len(h).max(1e-6));
                            if *out {
                                n.i = mul(dir, other);
                            } else {
                                n.o = mul(dir, other);
                            }
                        }
                    }
                }
                ui.want_cursor(Cursor::Crosshair);
            }
            Drag::PenNode { id, sub: si, node, from } => {
                let p = self.snap_pt(m);
                if dist(p, *from) * self.zoom > 3.0 {
                    if let Some(Geom::Path(subs)) = self.drawing.get_mut(*id).map(|s| &mut s.geom) {
                        if let Some(n) = subs.get_mut(*si).and_then(|s| s.nodes.get_mut(*node)) {
                            n.o = sub(p, n.p);
                            n.i = mul(n.o, -1.0);
                            n.smooth = true;
                        }
                    }
                }
            }
            Drag::Band { .. } | Drag::NodeBand { .. } => {}
        }
        self.drag = Some(drag);
    }

    fn drag_end(&mut self, ui: &mut Ui<'_>, m: P) {
        let Some(drag) = self.drag.take() else { return };
        match drag {
            Drag::Create { id, .. } => {
                let tiny = self.drawing.get(id).map_or(true, |s| {
                    let b = s.geom.bbox();
                    (b[2] - b[0]).max(b[3] - b[1]) * self.zoom < 3.0
                });
                if tiny {
                    self.drawing.shapes.retain(|s| s.id != id);
                    self.sel.clear();
                }
            }
            Drag::Band { from } => {
                let r = [from[0].min(m[0]), from[1].min(m[1]), from[0].max(m[0]), from[1].max(m[1])];
                if (r[2] - r[0]) * self.zoom > 3.0 || (r[3] - r[1]) * self.zoom > 3.0 {
                    for s in &self.drawing.shapes {
                        let b = s.geom.bbox();
                        if b[0] >= r[0] && b[1] >= r[1] && b[2] <= r[2] && b[3] <= r[3] && !self.sel.contains(&s.id) {
                            self.sel.push(s.id);
                        }
                    }
                }
            }
            Drag::NodeBand { from } => {
                let r = [from[0].min(m[0]), from[1].min(m[1]), from[0].max(m[0]), from[1].max(m[1])];
                if let Some(Geom::Path(subs)) = self.sel.first().and_then(|id| self.drawing.get(*id)).map(|s| s.geom.clone()) {
                    for (si, sub) in subs.iter().enumerate() {
                        for (ni, n) in sub.nodes.iter().enumerate() {
                            if n.p[0] >= r[0] && n.p[0] <= r[2] && n.p[1] >= r[1] && n.p[1] <= r[3] {
                                self.nsel.insert((si, ni));
                            }
                        }
                    }
                }
            }
            Drag::Pencil { pts } => {
                if pts.len() > 2 {
                    let eps = 2.5 / self.zoom;
                    let s = simplify(&pts, eps);
                    if s.len() >= 2 {
                        let nodes = fit_nodes(&s);
                        let id = self.drawing.push("Path", Geom::Path(vec![Sub { nodes, closed: false }]), self.line_style());
                        self.sel = vec![id];
                    }
                }
            }
            _ => {}
        }
    }

    // ----------------------------------------------------------------------------------------------- paint

    fn paint(&mut self, ui: &mut Ui<'_>, area: [f32; 4], scale: f32) {
        let (pw, ph) = ((area[2] * scale).round().max(1.0) as u32, (area[3] * scale).round().max(1.0) as u32);
        let ov = self.render_overlay(ui, pw, ph, scale);
        self.over = ov.map(Arc::new);
        if let Some(pic) = &self.over {
            ui.picture(area, pic, [0.0, 0.0, pw as f32, ph as f32], 1.0);
        }
        if self.rulers {
            self.draw_rulers(ui);
        }
    }

    /// The rulers: ticks every few canvas units (as many as the zoom allows), numbers at the long ones and the pointer
    /// marked.
    fn draw_rulers(&self, ui: &mut Ui<'_>) {
        let (ink, acc) = (ui.ink, ui.accent);
        let (top, left) = self.ruler_rects();
        let t = RULER;
        ui.fill(top, 0.0, ink, 0.1);
        ui.fill(left, 0.0, ink, 0.1);
        let (label, minor) = [(1.0, 1.0), (2.0, 1.0), (5.0, 1.0), (10.0, 2.0), (20.0, 5.0), (50.0, 10.0), (100.0, 20.0), (200.0, 50.0), (500.0, 100.0), (1000.0, 200.0)]
            .into_iter()
            .find(|(l, _)| l * self.zoom >= 56.0)
            .unwrap_or((1000.0, 200.0));
        let steps = (SIZE / minor).floor() as i32;
        for i in 0..=steps {
            let pos = i as f32 * minor;
            let (sx, sy) = (self.to_screen([pos, 0.0])[0], self.to_screen([0.0, pos])[1]);
            let major = (pos / label - (pos / label).round()).abs() < 1e-4;
            let len = if major { t * 0.65 } else { t * 0.3 };
            ui.fill([sx, top[1] + t - len, 1.0, len], 0.0, ink, 0.45);
            ui.fill([left[0] + t - len, sy, len, 1.0], 0.0, ink, 0.45);
            if major {
                let text = format!("{}", pos as i32);
                ui.text(sx + 3.0, top[1] + 8.5, &text, 8.5, false, ink, 0.6, Align::Left);
                ui.text(left[0] + 1.5, sy - 2.0, &text, 8.5, false, ink, 0.6, Align::Left);
            }
        }
        let m = ui.mouse;
        let c = self.to_canvas(m);
        if (0.0..=SIZE).contains(&c[0]) && (0.0..=SIZE).contains(&c[1]) {
            ui.fill([m[0], top[1], 1.0, t], 0.0, acc, 0.9);
            ui.fill([left[0], m[1], t, 1.0], 0.0, acc, 0.9);
        }
    }

    fn render_overlay(&self, ui: &Ui<'_>, pw: u32, ph: u32, scale: f32) -> Option<Pic> {
        let mut pm = tiny_skia::Pixmap::new(pw, ph)?;
        let acc = ui.accent;
        let mut ov = Overlay { pm: &mut pm, s: scale, ox: self.view[0], oy: self.view[1] };
        let mouse_c = self.to_canvas(ui.mouse);
        // the outlines of the selected shapes
        for s in self.selected() {
            for (pts, closed) in s.geom.polylines() {
                let sp: Vec<P> = pts.iter().map(|p| self.to_screen(*p)).collect();
                ov.poly(&sp, closed, acc, 0.9, 1.2);
            }
        }
        match self.tool {
            Tool::Select => {
                if let Some((b, pts, rot)) = self.handles() {
                    ov.rect(self.to_screen([b[0], b[1]]), self.to_screen([b[2], b[3]]), acc, 0.7, 1.0);
                    let top = self.to_screen([(b[0] + b[2]) / 2.0, b[1]]);
                    ov.line(top, self.to_screen(rot), acc, 0.7, 1.0);
                    ov.circle(self.to_screen(rot), 5.0, [1.0; 3], acc);
                    for p in pts {
                        ov.square(self.to_screen(p), 4.5, [1.0; 3], acc);
                    }
                }
                if self.drag.is_none() {
                    if let Some(s) = self.drawing.pick(mouse_c, 6.0 / self.zoom).and_then(|id| self.drawing.get(id)) {
                        if !self.sel.contains(&s.id) {
                            for (pts, closed) in s.geom.polylines() {
                                let sp: Vec<P> = pts.iter().map(|p| self.to_screen(*p)).collect();
                                ov.poly(&sp, closed, acc, 0.5, 1.5);
                            }
                        }
                    }
                }
            }
            Tool::Node => {
                if let Some(Geom::Path(subs)) = self.sel.first().and_then(|id| self.drawing.get(*id)).map(|s| s.geom.clone()) {
                    for (si, sub) in subs.iter().enumerate() {
                        for (ni, n) in sub.nodes.iter().enumerate() {
                            let on = self.nsel.contains(&(si, ni));
                            if on {
                                for h in [n.i, n.o] {
                                    if len(h) > 1e-3 {
                                        let hp = self.to_screen(add(n.p, h));
                                        ov.line(self.to_screen(n.p), hp, acc, 0.8, 1.0);
                                        ov.circle(hp, 3.5, [1.0; 3], acc);
                                    }
                                }
                            }
                        }
                    }
                    for (si, sub) in subs.iter().enumerate() {
                        for (ni, n) in sub.nodes.iter().enumerate() {
                            let on = self.nsel.contains(&(si, ni));
                            let p = self.to_screen(n.p);
                            let (fill, line) = if on { (acc, [1.0; 3]) } else { ([1.0; 3], acc) };
                            if n.smooth { ov.circle(p, 5.0, fill, line) } else { ov.square(p, 4.5, fill, line) }
                        }
                    }
                }
            }
            Tool::Pen => {
                if let Some((id, si)) = self.pen {
                    if let Some(Geom::Path(subs)) = self.drawing.get(id).map(|s| &s.geom) {
                        if let Some(last) = subs.get(si).and_then(|s| s.nodes.last()) {
                            let c = [last.p, add(last.p, last.o), self.snap_pt(mouse_c), self.snap_pt(mouse_c)];
                            let pts: Vec<P> = (0..=16).map(|k| self.to_screen(bez(&c, k as f32 / 16.0))).collect();
                            ov.poly(&pts, false, acc, 0.8, 1.2);
                        }
                        for n in subs.get(si).map(|s| s.nodes.as_slice()).unwrap_or(&[]) {
                            ov.square(self.to_screen(n.p), 3.5, [1.0; 3], acc);
                        }
                    }
                }
            }
            _ => {}
        }
        match &self.drag {
            Some(Drag::Band { from }) | Some(Drag::NodeBand { from }) => {
                ov.rect_fill(self.to_screen(*from), self.to_screen(mouse_c), acc, 0.12);
                ov.rect(self.to_screen(*from), self.to_screen(mouse_c), acc, 0.9, 1.0);
            }
            Some(Drag::Pencil { pts }) => {
                let sp: Vec<P> = pts.iter().map(|p| self.to_screen(*p)).collect();
                ov.poly(&sp, false, acc, 0.9, 1.5);
            }
            _ => {}
        }
        // the grid, the guides, and the lines something lines up with
        if self.grid > 0.0 && self.grid * self.zoom >= 4.0 {
            let mut g = 0.0;
            while g <= SIZE + 0.01 {
                ov.line(self.to_screen([g, 0.0]), self.to_screen([g, SIZE]), acc, 0.14, 1.0);
                ov.line(self.to_screen([0.0, g]), self.to_screen([SIZE, g]), acc, 0.14, 1.0);
                g += self.grid;
            }
        }
        let (tl, br) = (self.to_canvas([self.view[0], self.view[1]]), self.to_canvas([self.view[0] + self.view[2], self.view[1] + self.view[3]]));
        for (vertical, pos) in self.guides.iter().chain(self.guide_drag.iter()) {
            let (a, b) = if *vertical { ([*pos, tl[1]], [*pos, br[1]]) } else { ([tl[0], *pos], [br[0], *pos]) };
            ov.line(self.to_screen(a), self.to_screen(b), [0.0, 0.72, 0.9], 0.9, 1.0);
        }
        for (vertical, pos, lo, hi) in self.active_guides(mouse_c) {
            let (a, b) = if vertical { ([pos, lo], [pos, hi]) } else { ([lo, pos], [hi, pos]) };
            ov.line(self.to_screen(a), self.to_screen(b), [1.0, 0.2, 0.55], 0.95, 1.0);
        }
        Some(pic_of(&pm))
    }

    // ---------------------------------------------------------------------------------------------- panel

    /// The side panel, laid out like the composer's own inspector: a header, then rows that scroll under it. Returns the
    /// colour the picker is wanted for, with the y of its row.
    pub fn panel(&mut self, ui: &mut Ui<'_>, area: [f32; 4]) -> Option<(Target, C, f32)> {
        use super::{blob_rows, reset_link, section_icon, swatch};
        let (ink, accent) = (ui.ink, ui.accent);
        let x = area[0] + 20.0;
        let cw = area[2] - 40.0;
        let mut want: Option<(Target, C, f32)> = None;

        // the header
        let (_, glyph_tip, _) = Tool::ALL.iter().find(|t| t.0 == self.tool).map(|t| (t.0, t.2, t.3)).unwrap_or((Tool::Select, "", ""));
        ui.text(x, area[1] + 28.0, glyph_tip, 15.0, true, ink, 0.95, Align::Left);
        let count = self.drawing.shapes.len();
        let sub = if self.sel.is_empty() { format!("{count} object{}", if count == 1 { "" } else { "s" }) } else { format!("{} selected of {count}", self.sel.len()) };
        ui.text(x + cw, area[1] + 28.0, &sub, 11.5, true, accent, 0.9, Align::Right);
        let top = area[1] + 44.0;

        // the settings scroll under the header
        let list = [area[0] + 1.0, top, area[2] - 1.0, area[1] + area[3] - top];
        let max_scroll = (self.panel_h - list[3] + 12.0).max(0.0);
        if ui.scroll != 0.0 && inside(ui.mouse, list) {
            self.scroll_ins += ui.scroll;
        }
        self.scroll_ins = self.scroll_ins.clamp(0.0, max_scroll);
        let off = ui.ease(9_003, self.scroll_ins, 18.0);
        // what is scrolled out of the list takes no clicks
        let (mouse, click) = (ui.mouse, ui.click);
        if !inside(ui.mouse, list) {
            ui.mouse = [-1000.0; 2];
            ui.click = false;
        }
        ui.push_clip(list);
        let mut f = chebrium::form::Form::new(x, list[1] + 14.0 - off, cw, 36.0);
        let start = f.y;

        // options of the tool
        match self.tool {
            Tool::Star => {
                section_icon(ui, x, f.y - 4.0, "star", "Star");
                f.skip(18.0);
                let mut star = usize::from(self.star);
                ui.row(f.x, f.y, "Shape");
                ui.segmented([f.x + 110.0, f.y - 15.0, f.cw - 110.0, 30.0], &["Polygon", "Star"], &mut star);
                self.star = star == 1;
                f.skip(36.0);
                let mut c = self.corners as f32;
                f.slider(ui, "Corners", &mut c, 3.0, 16.0, "");
                self.corners = c.round() as usize;
                if self.star {
                    f.slider(ui, "Ratio", &mut self.ratio, 0.1, 0.95, "");
                }
                f.skip(8.0);
            }
            Tool::Node => {
                section_icon(ui, x, f.y - 4.0, "spline", "Nodes");
                f.skip(18.0);
                let btns: [(&str, u8); 6] = [("spline", 0), ("triangle", 1), ("minus", 2), ("plus", 3), ("trash-2", 4), ("circle-dot", 5)];
                let names = ["Smooth", "Cusp", "Straight", "Insert", "Delete", "Open / close"];
                let w = (cw - 5.0 * 6.0) / 6.0;
                for (i, (g, act)) in btns.iter().enumerate() {
                    let r = [x + i as f32 * (w + 6.0), f.y - 14.0, w, 32.0];
                    if tool_button(ui, r, g, false) {
                        match act {
                            0 => self.nodes_smooth(),
                            1 => self.nodes_cusp(),
                            2 => self.nodes_straight(),
                            3 => self.nodes_insert(),
                            4 => self.nodes_delete(),
                            _ => self.nodes_toggle_closed(),
                        }
                    }
                    if ui.hit(r) {
                        self.status = names[i].to_string();
                    }
                }
                f.skip(44.0);
            }
            _ => {}
        }

        let style = self.selected().first().map(|s| s.style).unwrap_or(self.cur);

        // fill
        section_icon(ui, x, f.y - 4.0, "palette", "Fill");
        f.skip(18.0);
        {
            let mut k = style.fill.kind();
            ui.row(f.x, f.y, "Paint");
            if ui.segmented([f.x + 110.0, f.y - 15.0, f.cw - 110.0, 30.0], &["None", "Flat", "Linear", "Radial", "Blobs"], &mut k) {
                let base = style.fill.first().unwrap_or(self.cur.fill.first().unwrap_or([0.25, 0.52, 0.98]));
                let second = if style.fill.kind() >= 2 { style.fill.second() } else { shift_hue(base) };
                self.set_style(|s| {
                    s.fill = match k {
                        0 => Fill::None,
                        1 => Fill::Solid(base),
                        2 => Fill::Linear(base, second, 90.0),
                        3 => Fill::Radial(base, second),
                        _ => Fill::Blobs(base, Blobs::starter(base, second)),
                    }
                });
            }
            f.skip(36.0);
            match style.fill {
                Fill::None => {}
                Fill::Solid(c) => {
                    let ay = f.y;
                    if swatch(ui, &mut f, "Colour", Some(c)) {
                        want = Some((Target::Fill, c, ay));
                    }
                }
                Fill::Linear(a, b, mut deg) => {
                    let ay = f.y;
                    if swatch(ui, &mut f, "From", Some(a)) {
                        want = Some((Target::Fill, a, ay));
                    }
                    let ay = f.y;
                    if swatch(ui, &mut f, "To", Some(b)) {
                        want = Some((Target::Fill2, b, ay));
                    }
                    let before = deg;
                    f.slider(ui, "Angle", &mut deg, 0.0, 360.0, "°");
                    if (deg - before).abs() > 1e-3 {
                        self.set_style(|s| s.fill = Fill::Linear(a, b, deg));
                    }
                }
                Fill::Radial(a, b) => {
                    let ay = f.y;
                    if swatch(ui, &mut f, "Centre", Some(a)) {
                        want = Some((Target::Fill, a, ay));
                    }
                    let ay = f.y;
                    if swatch(ui, &mut f, "Edge", Some(b)) {
                        want = Some((Target::Fill2, b, ay));
                    }
                }
                Fill::Blobs(base, mut k) => {
                    let ay = f.y;
                    if swatch(ui, &mut f, "Base", Some(base)) {
                        want = Some((Target::Fill, base, ay));
                    }
                    let (changed, ask) = blob_rows(ui, &mut f, &mut k, &mut self.blob_sel);
                    if let Some((i, c, y)) = ask {
                        want = Some((Target::Blob(i), c, y));
                    }
                    if changed {
                        self.set_style(|s| s.fill = Fill::Blobs(base, k));
                    }
                }
            }
        }
        let mut o = style.opacity * 100.0;
        f.slider(ui, "Opacity", &mut o, 0.0, 100.0, "%");
        if ((o / 100.0) - style.opacity).abs() > 1e-4 {
            self.set_style(|s| s.opacity = o / 100.0);
        }
        f.skip(10.0);

        // stroke
        section_icon(ui, x, f.y - 4.0, "pen-tool", "Stroke");
        f.skip(18.0);
        {
            let mut k = usize::from(style.stroke.is_some());
            ui.row(f.x, f.y, "Line");
            if ui.segmented([f.x + 110.0, f.y - 15.0, f.cw - 110.0, 30.0], &["None", "Solid"], &mut k) {
                let c = style.stroke.unwrap_or_else(|| style.fill.first().map(shade).unwrap_or([0.1; 3]));
                self.set_style(|s| {
                    s.stroke = if k == 1 { Some(c) } else { None };
                    if k == 1 && s.width < 0.5 {
                        s.width = 8.0;
                    }
                });
            }
            f.skip(36.0);
            if let Some(c) = style.stroke {
                let ay = f.y;
                if swatch(ui, &mut f, "Colour", Some(c)) {
                    want = Some((Target::Stroke, c, ay));
                }
                let mut w = style.width;
                f.slider(ui, "Width", &mut w, 0.0, 80.0, "");
                if (w - style.width).abs() > 1e-3 {
                    self.set_style(|s| s.width = w);
                }
                let (mut cap, mut join) = (style.cap, style.join);
                ui.row(f.x, f.y, "Cap");
                if ui.segmented([f.x + 110.0, f.y - 15.0, f.cw - 110.0, 30.0], &["Butt", "Round", "Square"], &mut cap) {
                    self.set_style(|s| s.cap = cap);
                }
                f.skip(36.0);
                ui.row(f.x, f.y, "Join");
                if ui.segmented([f.x + 110.0, f.y - 15.0, f.cw - 110.0, 30.0], &["Miter", "Round", "Bevel"], &mut join) {
                    self.set_style(|s| s.join = join);
                }
                f.skip(36.0);
            }
        }
        f.skip(10.0);

        // geometry of the selection
        if let Some(b) = self.selection_box() {
            section_icon(ui, x, f.y - 4.0, "move", "Geometry");
            f.skip(18.0);
            let mut v = [b[0], b[1], b[2] - b[0], b[3] - b[1]];
            let old = v;
            f.slider(ui, "X", &mut v[0], -128.0, SIZE, "");
            f.slider(ui, "Y", &mut v[1], -128.0, SIZE, "");
            f.slider(ui, "Width", &mut v[2], 1.0, SIZE, "");
            f.slider(ui, "Height", &mut v[3], 1.0, SIZE, "");
            if v.iter().zip(old).any(|(a, b)| (a - b).abs() > 1e-3) {
                self.set_box(v);
            }
            if self.sel.len() == 1 {
                let id = self.sel[0];
                let mut radius = None;
                if let Some(Shape { geom: Geom::Rect(r, rad), .. }) = self.drawing.get(id) {
                    radius = Some((*rad, r[2].min(r[3]) / 2.0));
                }
                if let Some((rad, max)) = radius {
                    let mut r = rad;
                    f.slider(ui, "Corner radius", &mut r, 0.0, max.max(1.0), "");
                    if (r - rad).abs() > 1e-3 {
                        if let Some(Shape { geom: Geom::Rect(_, rr), .. }) = self.drawing.get_mut(id) {
                            *rr = r;
                        }
                        self.touch();
                    }
                }
            }
            f.skip(10.0);

            section_icon(ui, x, f.y - 4.0, "layers-2", "Arrange");
            f.skip(18.0);
            let row1: [(&str, &str, u8); 8] = [("arrow-up", "To front (Shift: top)", 0), ("arrow-down", "To back (Shift: bottom)", 1), ("flip-horizontal-2", "Flip horizontally", 2), ("flip-vertical-2", "Flip vertically", 3), ("rotate-ccw", "Turn 90° (Shift: other way)", 4), ("copy", "Duplicate", 5), ("trash-2", "Delete", 6), ("spline", "Object to path", 7)];
            let w = (cw - 7.0 * 5.0) / 8.0;
            for (i, (g, tip, act)) in row1.iter().enumerate() {
                let r = [x + i as f32 * (w + 5.0), f.y - 14.0, w, 32.0];
                if tool_button(ui, r, g, false) {
                    match act {
                        0 => self.restack(true, ui.mods.shift),
                        1 => self.restack(false, ui.mods.shift),
                        2 => self.flip(true),
                        3 => self.flip(false),
                        4 => self.rotate_by(if ui.mods.shift { -90.0 } else { 90.0 }),
                        5 => self.duplicate(),
                        6 => self.delete_selected(),
                        _ => self.to_paths(),
                    }
                }
                if ui.hit(r) {
                    self.status = tip.to_string();
                }
            }
            f.skip(40.0);
            let row2 = ["align-start-vertical", "align-center-vertical", "align-end-vertical", "align-start-horizontal", "align-center-horizontal", "align-end-horizontal"];
            let names = ["Align left", "Centre horizontally", "Align right", "Align top", "Centre vertically", "Align bottom"];
            let w = (cw - 5.0 * 5.0) / 6.0;
            for (i, g) in row2.iter().enumerate() {
                let r = [x + i as f32 * (w + 5.0), f.y - 14.0, w, 32.0];
                if tool_button(ui, r, g, false) {
                    self.align(i);
                }
                if ui.hit(r) {
                    self.status = names[i].to_string();
                }
            }
            f.skip(48.0);
        }

        // rulers, guides and snapping
        section_icon(ui, x, f.y - 4.0, "ruler", "Guides");
        f.skip(18.0);
        f.toggle(ui, "Smart guides", &mut self.smart);
        f.toggle(ui, "Rulers", &mut self.rulers);
        f.toggle(ui, "Snap to grid", &mut self.snap);
        f.slider(ui, "Grid", &mut self.grid, 0.0, 64.0, "");
        self.grid = self.grid.round();
        if !self.guides.is_empty() {
            let n = self.guides.len();
            if ui.button([f.x, f.y - 15.0, f.cw, 30.0], &format!("Clear {n} guide{}", if n == 1 { "" } else { "s" }), false) {
                self.guides.clear();
            }
            f.skip(36.0);
        } else if self.rulers {
            f.note(ui, "Drag from a ruler to pull out a guide.", true);
        }
        f.skip(10.0);

        // the objects, front first
        section_icon(ui, x, f.y - 4.0, "layers", "Objects");
        f.skip(18.0);
        if self.drawing.shapes.is_empty() {
            f.note(ui, "Nothing drawn yet. Pick a tool and drag on the canvas.", true);
        }
        let ids: Vec<(u32, String, Style)> = self.drawing.shapes.iter().rev().map(|s| (s.id, s.name.clone(), s.style)).collect();
        for (id, name, st) in ids {
            let r = [x, f.y - 16.0, cw, 32.0];
            let on = self.sel.contains(&id);
            let hov = ui.hit(r);
            let k = ui.ease(0x7ec0_0000 + id as u64, if on { 1.0 } else if hov { 0.5 } else { 0.0 }, 22.0);
            ui.fill(r, 10.0, if on { accent } else { ink }, if on { 0.3 * k } else { 0.08 * k });
            let swatch_c = st.fill.first().or(st.stroke).unwrap_or([0.5; 3]);
            ui.fill([r[0] + 8.0, r[1] + 8.0, 16.0, 16.0], 5.0, swatch_c, 1.0);
            ui.text_raw(r[0] + 34.0, r[1] + 21.0, &name, 13.0, false, ink, 0.9, Align::Left);
            if hov {
                ui.want_cursor(Cursor::Pointer);
                if ui.click {
                    if ui.mods.shift {
                        if let Some(at) = self.sel.iter().position(|s| *s == id) {
                            self.sel.remove(at);
                        } else {
                            self.sel.push(id);
                        }
                    } else {
                        self.sel = vec![id];
                    }
                    self.nsel.clear();
                }
            }
            f.skip(36.0);
        }
        self.panel_h = f.y - start + 40.0;
        ui.pop_clip();
        ui.mouse = mouse;
        ui.click = click;
        let _ = reset_link;
        want
    }

    /// The toolbox: a column of tool buttons at `(x, y)`. Returns the height used.
    pub fn toolbox(&mut self, ui: &mut Ui<'_>, x: f32, y: f32) -> f32 {
        let ink = ui.ink;
        let n = Tool::ALL.len() as f32;
        let panel = [x, y, 48.0, n * 40.0 + 8.0 + 18.0];
        ui.fill(panel, 16.0, ink, 0.09);
        for (i, (tool, glyph, tip, _)) in Tool::ALL.iter().enumerate() {
            let r = [x + 4.0, y + 4.0 + i as f32 * 40.0, 40.0, 36.0];
            let on = self.tool == *tool;
            if tool_button(ui, r, glyph, on) {
                self.set_tool(*tool);
            }
            if ui.hit(r) {
                self.status = (*tip).to_string();
            }
        }
        panel[3]
    }
}

/// A square for the panel with an icon: true when pressed.
fn tool_button(ui: &mut Ui<'_>, r: [f32; 4], glyph: &str, on: bool) -> bool {
    let (ink, accent) = (ui.ink, ui.accent);
    let hov = ui.hit(r);
    let key = 0x7ec1_0000_0000 + ((r[0] as u64) << 16) + r[1] as u64;
    let k = ui.ease(key, if on { 1.0 } else if hov { 0.5 } else { 0.0 }, 22.0);
    ui.fill(r, 10.0, if on { accent } else { ink }, if on { 0.9 } else { 0.1 * k });
    ui.glyph(r[0] + r[2] / 2.0, r[1] + r[3] / 2.0, 17.0, glyph, if on { [1.0; 3] } else { ink }, 0.9);
    if hov {
        ui.want_cursor(Cursor::Pointer);
    }
    hov && ui.click
}

fn colour_swatch(ui: &mut Ui<'_>, r: [f32; 4], c: C) -> bool {
    let hov = ui.hit(r);
    let ink = ui.ink;
    ui.fill(r, 9.0, ink, if hov { 0.35 } else { 0.2 });
    ui.fill([r[0] + 1.0, r[1] + 1.0, r[2] - 2.0, r[3] - 2.0], 8.0, c, 1.0);
    let light = 0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2] > 0.6;
    ui.text_raw(r[0] + 12.0, r[1] + 20.0, &to_hex(c), 12.5, true, if light { [0.08; 3] } else { [1.0; 3] }, 0.9, Align::Left);
    if hov {
        ui.want_cursor(Cursor::Pointer);
    }
    hov && ui.click
}

fn shift_hue(c: C) -> C {
    let hsv = memrium_de::colour::rgb_to_hsv(c);
    hsv_to_rgb([(hsv[0] + 40.0) % 360.0, hsv[1], (hsv[2] * 0.75).max(0.2)])
}

fn shade(c: C) -> C {
    [c[0] * 0.55, c[1] * 0.55, c[2] * 0.55]
}

fn to_screen_at(view: [f32; 4], pan: P, zoom: f32, p: P) -> P {
    [view[0] + pan[0] + p[0] * zoom, view[1] + pan[1] + p[1] * zoom]
}

fn sub_pts(a: P, b: P) -> P {
    sub(a, b)
}

fn bbox_union(shapes: &[Shape]) -> [f32; 4] {
    let mut o = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
    for s in shapes {
        let b = s.geom.bbox();
        o = [o[0].min(b[0]), o[1].min(b[1]), o[2].max(b[2]), o[3].max(b[3])];
    }
    o
}

/// Smooth bezier nodes through a polyline (Catmull-Rom tangents).
fn fit_nodes(s: &[P]) -> Vec<Node> {
    let n = s.len();
    (0..n)
        .map(|i| {
            let prev = s[i.saturating_sub(1)];
            let next = s[(i + 1).min(n - 1)];
            if i == 0 || i == n - 1 {
                let t = if i == 0 { mul(sub(next, s[i]), 1.0 / 3.0) } else { mul(sub(prev, s[i]), 1.0 / 3.0) };
                Node { p: s[i], i: if i == n - 1 { t } else { [0.0; 2] }, o: if i == 0 { t } else { [0.0; 2] }, smooth: false }
            } else {
                let t = mul(sub(next, prev), 1.0 / 6.0);
                Node { p: s[i], i: mul(t, -1.0), o: t, smooth: true }
            }
        })
        .collect()
}

fn star_path(c: P, r: f32, a0: f32, corners: usize, ratio: f32, star: bool) -> Sub {
    let n = corners.max(3);
    let count = if star { n * 2 } else { n };
    let nodes = (0..count)
        .map(|i| {
            let rr = if star && i % 2 == 1 { r * ratio } else { r };
            let a = a0 + std::f32::consts::TAU * i as f32 / count as f32;
            Node::corner([c[0] + rr * a.cos(), c[1] + rr * a.sin()])
        })
        .collect();
    Sub { nodes, closed: true }
}

impl Sub {
    /// Fewer nodes for nearly the same line: the outline is thinned and fitted again.
    fn simplified(&self) -> Sub {
        let pts = self.flatten();
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for p in &pts {
            x0 = x0.min(p[0]);
            y0 = y0.min(p[1]);
            x1 = x1.max(p[0]);
            y1 = y1.max(p[1]);
        }
        let eps = ((x1 - x0).hypot(y1 - y0) * 0.006).max(0.6);
        let s = simplify(&pts, eps);
        if s.len() < 2 {
            return self.clone();
        }
        let mut nodes = fit_nodes(&s);
        if self.closed {
            if nodes.len() < 4 {
                return self.clone();
            }
            // the outline came back to its start: the last node is the first one again
            let last = nodes.pop().unwrap();
            nodes[0].i = last.i;
        }
        if nodes.len() >= self.nodes.len() {
            // nothing to gain: keep the nodes as they are
            return self.clone();
        }
        Sub { nodes, closed: self.closed }
    }

    /// Round the sharp corners among `which` (all when empty) that lie between two straight segments.
    fn round_corners(&mut self, which: &HashSet<usize>, r: f32) {
        let orig = self.nodes.clone();
        let n = orig.len();
        let mut out = Vec::with_capacity(n * 2);
        for i in 0..n {
            let skip = (!which.is_empty() && !which.contains(&i)) || (!self.closed && (i == 0 || i == n - 1));
            let (prev, cur, next) = (orig[(i + n - 1) % n], orig[i], orig[(i + 1) % n]);
            let sharp = len(cur.i) < 1e-3 && len(cur.o) < 1e-3 && len(prev.o) < 1e-3 && len(next.i) < 1e-3;
            let (dp, dn) = (dist(prev.p, cur.p), dist(next.p, cur.p));
            let d = r.min(dp * 0.5).min(dn * 0.5);
            if skip || !sharp || d < 0.5 {
                out.push(cur);
                continue;
            }
            let a = add(cur.p, mul(sub(prev.p, cur.p), d / dp));
            let b = add(cur.p, mul(sub(next.p, cur.p), d / dn));
            out.push(Node { p: a, i: [0.0; 2], o: mul(sub(cur.p, a), KAPPA), smooth: false });
            out.push(Node { p: b, i: mul(sub(cur.p, b), KAPPA), o: [0.0; 2], smooth: false });
        }
        self.nodes = out;
    }

    fn reverse(&mut self) {
        self.nodes.reverse();
        for n in &mut self.nodes {
            std::mem::swap(&mut n.i, &mut n.o);
        }
    }

    /// Shake the nodes among `which` (all when empty) a little, the same way every time.
    fn roughen(&mut self, which: &HashSet<usize>, amount: f32) {
        let noise = |k: u32| -> f32 {
            let mut h = k.wrapping_mul(0x9E37_79B1) ^ 0x85EB_CA6B;
            h = (h ^ (h >> 15)).wrapping_mul(0x2C1B_3C6D);
            ((h >> 8) & 0xFFFF) as f32 / 32768.0 - 1.0
        };
        for (i, n) in self.nodes.iter_mut().enumerate() {
            if which.is_empty() || which.contains(&i) {
                n.p = add(n.p, [noise(i as u32 * 2) * amount, noise(i as u32 * 2 + 1) * amount]);
            }
        }
    }

    /// Cut segment `i` at `t`; the new node is number `i + 1`.
    fn insert_at(&mut self, i: usize, t: f32) {
        let n = self.nodes.len();
        let j = (i + 1) % n;
        let c = self.seg(i);
        let line = self.is_line(i);
        let (l, r) = split(&c, t);
        let q = Node { p: l[3], i: sub(l[2], l[3]), o: sub(r[1], r[0]), smooth: !line };
        self.nodes[i].o = sub(l[1], l[0]);
        self.nodes[j].i = sub(r[2], r[3]);
        self.nodes.insert(i + 1, q);
    }
}

fn pic_of(pm: &tiny_skia::Pixmap) -> Pic {
    let mut data = pm.data().to_vec();
    for p in data.chunks_exact_mut(4) {
        let a = p[3] as f32 / 255.0;
        if a > 0.0 && a < 1.0 {
            for c in &mut p[..3] {
                *c = ((*c as f32 / a).min(255.0)) as u8;
            }
        }
    }
    Pic::from_rgba(pm.width(), pm.height(), &data)
}

/// Thin lines and handles drawn over the canvas into a transparent pixmap, in logical window pixels.
struct Overlay<'a> {
    pm: &'a mut tiny_skia::Pixmap,
    s: f32,
    ox: f32,
    oy: f32,
}

impl Overlay<'_> {
    fn paint(c: C, a: f32) -> tiny_skia::Paint<'static> {
        let mut p = tiny_skia::Paint::default();
        p.set_color_rgba8((c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8, (a * 255.0) as u8);
        p.anti_alias = true;
        p
    }

    fn pt(&self, p: P) -> (f32, f32) {
        ((p[0] - self.ox) * self.s, (p[1] - self.oy) * self.s)
    }

    fn stroke(&mut self, path: Option<tiny_skia::Path>, c: C, a: f32, w: f32) {
        if let Some(path) = path {
            let st = tiny_skia::Stroke { width: w * self.s, line_join: tiny_skia::LineJoin::Round, ..Default::default() };
            self.pm.stroke_path(&path, &Self::paint(c, a), &st, tiny_skia::Transform::identity(), None);
        }
    }

    fn line(&mut self, a: P, b: P, c: C, alpha: f32, w: f32) {
        let mut pb = tiny_skia::PathBuilder::new();
        let (p, q) = (self.pt(a), self.pt(b));
        pb.move_to(p.0, p.1);
        pb.line_to(q.0, q.1);
        self.stroke(pb.finish(), c, alpha, w);
    }

    fn poly(&mut self, pts: &[P], closed: bool, c: C, a: f32, w: f32) {
        if pts.len() < 2 {
            return;
        }
        let mut pb = tiny_skia::PathBuilder::new();
        let p0 = self.pt(pts[0]);
        pb.move_to(p0.0, p0.1);
        for p in &pts[1..] {
            let q = self.pt(*p);
            pb.line_to(q.0, q.1);
        }
        if closed {
            pb.close();
        }
        self.stroke(pb.finish(), c, a, w);
    }

    fn rect(&mut self, a: P, b: P, c: C, alpha: f32, w: f32) {
        self.poly(&[a, [b[0], a[1]], b, [a[0], b[1]]], true, c, alpha, w);
    }

    fn rect_fill(&mut self, a: P, b: P, c: C, alpha: f32) {
        let (p, q) = (self.pt(a), self.pt(b));
        if let Some(r) = tiny_skia::Rect::from_ltrb(p.0.min(q.0), p.1.min(q.1), p.0.max(q.0).max(p.0.min(q.0) + 0.1), p.1.max(q.1).max(p.1.min(q.1) + 0.1)) {
            self.pm.fill_rect(r, &Self::paint(c, alpha), tiny_skia::Transform::identity(), None);
        }
    }

    fn square(&mut self, c: P, half: f32, fill: C, line: C) {
        let (x, y) = self.pt(c);
        let h = half * self.s;
        if let Some(r) = tiny_skia::Rect::from_ltrb(x - h, y - h, x + h, y + h) {
            self.pm.fill_rect(r, &Self::paint(fill, 1.0), tiny_skia::Transform::identity(), None);
            let path = tiny_skia::PathBuilder::from_rect(r);
            let st = tiny_skia::Stroke { width: 1.4 * self.s, ..Default::default() };
            self.pm.stroke_path(&path, &Self::paint(line, 1.0), &st, tiny_skia::Transform::identity(), None);
        }
    }

    fn circle(&mut self, c: P, r: f32, fill: C, line: C) {
        let (x, y) = self.pt(c);
        if let Some(path) = tiny_skia::PathBuilder::from_circle(x, y, r * self.s) {
            self.pm.fill_path(&path, &Self::paint(fill, 1.0), tiny_skia::FillRule::Winding, tiny_skia::Transform::identity(), None);
            let st = tiny_skia::Stroke { width: 1.4 * self.s, ..Default::default() };
            self.pm.stroke_path(&path, &Self::paint(line, 1.0), &st, tiny_skia::Transform::identity(), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Drawing {
        let mut d = Drawing::default();
        d.push("Rectangle", Geom::Rect([100.0, 100.0, 200.0, 120.0], 20.0), Style::default());
        d.push("Ellipse", Geom::Ellipse([300.0, 300.0], [80.0, 50.0]), Style { fill: Fill::Linear([1.0, 0.0, 0.0], [0.0, 0.0, 1.0], 45.0), stroke: Some([0.0; 3]), ..Style::default() });
        d.push("Star", Geom::Path(vec![star_path([256.0, 256.0], 100.0, 0.0, 5, 0.5, true)]), Style::default());
        d
    }

    #[test]
    fn a_drawing_survives_a_trip_through_svg() {
        let d = sample();
        let svg = d.to_svg();
        let back = Drawing::from_svg(&svg).expect("reads its own svg");
        assert_eq!(back, d);
        // and it is a picture
        let img = memrium_de::svgedit::rasterize(&svg, 128).expect("renders");
        assert!(img.pixels().any(|p| p[3] > 200));
    }

    #[test]
    fn a_foreign_svg_becomes_editable_paths() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64" viewBox="0 0 64 64"><path d="M8 8 L56 8 L56 56 Q32 40 8 56 Z" fill="#ff0000"/><circle cx="32" cy="32" r="10" fill="none" stroke="#00ff00" stroke-width="4"/></svg>"##;
        let d = Drawing::from_svg(svg).unwrap();
        assert_eq!(d.shapes.len(), 2);
        let b = d.shapes[0].geom.bbox();
        // scaled from 64 to 512 units
        assert!((b[0] - 64.0).abs() < 1.0 && (b[2] - 448.0).abs() < 1.0, "{b:?}");
        assert_eq!(d.shapes[1].style.fill, Fill::None);
        assert!(d.shapes[1].style.stroke.is_some());
    }

    #[test]
    fn shapes_are_picked_by_fill_and_by_outline() {
        let d = sample();
        // inside the rectangle only
        assert_eq!(d.pick([120.0, 140.0], 4.0), Some(1));
        // the ellipse is above it
        assert_eq!(d.pick([300.0, 300.0], 4.0), Some(2));
        assert_eq!(d.pick([5.0, 5.0], 4.0), None);
    }

    #[test]
    fn transforms_keep_rectangles_rectangles_until_they_turn() {
        let mut g = Geom::Rect([10.0, 10.0, 100.0, 50.0], 0.0);
        g.transform(&scaling([0.0, 0.0], 2.0, 2.0));
        assert_eq!(g, Geom::Rect([20.0, 20.0, 200.0, 100.0], 0.0));
        g.transform(&rotation([100.0, 100.0], 0.5));
        assert!(matches!(g, Geom::Path(_)));
    }

    #[test]
    fn a_segment_is_cut_in_two_without_changing_its_shape() {
        let mut s = Sub {
            nodes: vec![Node { p: [0.0, 0.0], i: [0.0; 2], o: [0.0, 50.0], smooth: false }, Node { p: [100.0, 0.0], i: [0.0, 50.0], o: [0.0; 2], smooth: false }],
            closed: false,
        };
        let mid = bez(&s.seg(0), 0.5);
        s.insert_at(0, 0.5);
        assert_eq!(s.nodes.len(), 3);
        assert!(dist(s.nodes[1].p, mid) < 1e-3);
        let again = bez(&s.seg(0), 1.0);
        assert!(dist(again, mid) < 1e-3);
    }

    #[test]
    fn things_snap_to_the_edges_and_middles_of_other_objects() {
        let mut e = Editor::new();
        e.zoom = 1.0;
        e.drawing.push("a", Geom::Rect([100.0, 100.0, 80.0, 40.0], 0.0), Style::default());
        e.drawing.push("b", Geom::Rect([300.0, 300.0, 20.0, 20.0], 0.0), Style::default());
        e.sel = vec![e.drawing.shapes[1].id];
        // near the right edge of the first (x 180) and its middle (y 120)
        let p = e.snap_pt([182.5, 118.0]);
        assert_eq!(p, [180.0, 120.0]);
        // far from everything it stays where it is
        assert_eq!(e.snap_pt([230.0, 20.0]), [230.0, 20.0]);
        // the selected object is not a line for itself
        assert_eq!(e.snap_pt([301.0, 330.0]), [301.0, 330.0]);
        e.smart = false;
        assert_eq!(e.snap_pt([182.5, 118.0]), [182.5, 118.0]);
    }

    #[test]
    fn a_blob_fill_is_drawn_as_stacked_gradients_and_comes_back_from_the_file() {
        let mut d = Drawing::default();
        d.push("Blobby", Geom::Ellipse([256.0, 256.0], [100.0, 100.0]), Style { fill: Fill::Blobs([0.1, 0.1, 0.9], Blobs::starter([0.1, 0.1, 0.9], [1.0, 0.5, 0.0])), stroke: Some([0.0; 3]), ..Style::default() });
        let svg = d.to_svg();
        assert_eq!(svg.matches("<radialGradient").count(), 3, "one gradient for each blob");
        assert_eq!(svg.matches("<ellipse").count(), 5, "base, three blobs and the outline");
        assert_eq!(Drawing::from_svg(&svg).map(|d| d.shapes[0].style.fill), Some(d.shapes[0].style.fill));
    }

    #[test]
    fn freehand_points_are_simplified() {
        let pts: Vec<P> = (0..100).map(|i| [i as f32, 0.2 * (i as f32 * 0.5).sin()]).collect();
        let s = simplify(&pts, 1.0);
        assert!(s.len() < 10);
    }
}
