//! 3D model preview: OBJ (tobj), glTF / GLB (gltf), STL and PLY (small built-in parsers).
//!
//! Loading happens on a background thread (`load`). Drawing is done on the GPU through an egui_glow paint
//! callback, so there are no extra rendering dependencies and large meshes stay smooth. GL objects are created
//! lazily on the first paint (the callback is the only place that has the GL context) and freed on drop.
//!
//! Textures are optional: a model's own textures (OBJ/MTL `map_Kd`, glTF base colour) are used when the files are
//! found, and the "Textures" button in the status bar (or dropping images / a folder on the window) lets the user
//! supply their own. Only the colour / diffuse image is used (no normal / roughness maps), to stay light.
//!
//! Controls (see `HELP` for the list shown with H): left-drag orbit, right/middle/Shift-drag pan, wheel zoom,
//! arrow keys, +/-, R reset, 1/3/7 views, W wireframe, G grid, O ortho, Z up-axis, T turntable.

use std::{
    collections::HashMap,
    f32::consts::{FRAC_PI_2, PI},
    hash::{BuildHasherDefault, Hasher},
    path::{Path, PathBuf},
    sync::{
        mpsc::{self, Receiver, Sender, TryRecvError},
        Arc, Mutex,
    },
    thread,
};

use eframe::{
    egui::{self, vec2, Align2, Color32, FontId, Key, Rect, RichText, Sense},
    egui_glow,
    glow::{self, HasContext},
};

use crate::{icons, theme};

pub const MODEL_EXTS: &[&str] = &["obj", "stl", "ply", "gltf", "glb"];

const MAX_FILE_BYTES: u64 = 256 << 20;
const DEFAULT_COLOR: [f32; 3] = [0.78, 0.80, 0.84];

// ------------------------------------------------------------------------------------------
// Small vector / matrix helpers (column-major 4x4, like OpenGL). No math crate needed.
// ------------------------------------------------------------------------------------------

type V3 = [f32; 3];
type M4 = [f32; 16];

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}
fn dot(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn normalize_or(a: V3, fallback: V3) -> V3 {
    let l = dot(a, a).sqrt();
    if l > 1e-20 && l.is_finite() {
        [a[0] / l, a[1] / l, a[2] / l]
    } else {
        fallback
    }
}

fn identity() -> M4 {
    let mut m = [0.0; 16];
    m[0] = 1.0;
    m[5] = 1.0;
    m[10] = 1.0;
    m[15] = 1.0;
    m
}
fn mul(a: &M4, b: &M4) -> M4 {
    let mut r = [0.0; 16];
    for c in 0..4 {
        for row in 0..4 {
            let mut s = 0.0;
            for k in 0..4 {
                s += a[k * 4 + row] * b[c * 4 + k];
            }
            r[c * 4 + row] = s;
        }
    }
    r
}
fn translate(v: V3) -> M4 {
    let mut m = identity();
    m[12] = v[0];
    m[13] = v[1];
    m[14] = v[2];
    m
}
fn scale(s: f32) -> M4 {
    let mut m = identity();
    m[0] = s;
    m[5] = s;
    m[10] = s;
    m
}
fn rot_x(a: f32) -> M4 {
    let (s, c) = a.sin_cos();
    let mut m = identity();
    m[5] = c;
    m[6] = s;
    m[9] = -s;
    m[10] = c;
    m
}
fn perspective(fovy: f32, aspect: f32, near: f32, far: f32) -> M4 {
    let t = 1.0 / (fovy * 0.5).tan();
    let mut m = [0.0; 16];
    m[0] = t / aspect;
    m[5] = t;
    m[10] = (far + near) / (near - far);
    m[11] = -1.0;
    m[14] = 2.0 * far * near / (near - far);
    m
}
fn ortho(half_w: f32, half_h: f32, near: f32, far: f32) -> M4 {
    let mut m = [0.0; 16];
    m[0] = 1.0 / half_w;
    m[5] = 1.0 / half_h;
    m[10] = -2.0 / (far - near);
    m[14] = -(far + near) / (far - near);
    m[15] = 1.0;
    m
}
fn look_at(eye: V3, target: V3, up: V3) -> M4 {
    let f = normalize_or(sub(target, eye), [0.0, 0.0, -1.0]);
    let s = normalize_or(cross(f, up), [1.0, 0.0, 0.0]);
    let u = cross(s, f);
    let mut m = identity();
    m[0] = s[0];
    m[4] = s[1];
    m[8] = s[2];
    m[1] = u[0];
    m[5] = u[1];
    m[9] = u[2];
    m[2] = -f[0];
    m[6] = -f[1];
    m[10] = -f[2];
    m[12] = -dot(s, eye);
    m[13] = -dot(u, eye);
    m[14] = dot(f, eye);
    m
}
fn transform_point(m: &M4, p: V3) -> V3 {
    [
        m[0] * p[0] + m[4] * p[1] + m[8] * p[2] + m[12],
        m[1] * p[0] + m[5] * p[1] + m[9] * p[2] + m[13],
        m[2] * p[0] + m[6] * p[1] + m[10] * p[2] + m[14],
    ]
}
fn transform_dir(m: &M4, p: V3) -> V3 {
    normalize_or(
        [
            m[0] * p[0] + m[4] * p[1] + m[8] * p[2],
            m[1] * p[0] + m[5] * p[1] + m[9] * p[2],
            m[2] * p[0] + m[6] * p[1] + m[10] * p[2],
        ],
        [0.0, 1.0, 0.0],
    )
}

// ------------------------------------------------------------------------------------------
// Mesh + loaders (run off the UI thread)
// ------------------------------------------------------------------------------------------

/// One texture the model refers to (OBJ material / glTF image). `image` is filled when the file was found
/// next to the model or embedded in it; the user can supply it afterwards.
pub struct Slot {
    name: String,
    image: Option<ImageData>,
}

#[derive(Clone)]
pub struct ImageData {
    w: u32,
    h: u32,
    rgba: Vec<u8>,
}

/// A run of indices drawn with one texture slot.
#[derive(Clone, Copy)]
struct Part {
    first: u32,
    count: u32,
    slot: Option<usize>,
}

#[derive(Default)]
pub struct Mesh {
    pos: Vec<V3>,
    nrm: Vec<V3>,
    col: Vec<V3>,
    /// Texture coordinates in image space (0,0 = top-left), same length as `pos` when present.
    uv: Vec<[f32; 2]>,
    idx: Vec<u32>,
    parts: Vec<Part>,
}

impl Mesh {
    /// Append one triangle batch. Missing normals are computed (smooth). `color` multiplies the vertex colours.
    fn add(
        &mut self,
        pos: &[V3],
        nrm: Option<&[V3]>,
        vcol: Option<&[V3]>,
        color: V3,
        uv: Option<&[[f32; 2]]>,
        slot: Option<usize>,
        idx: &[u32],
    ) -> Result<(), String> {
        let idx = &idx[..idx.len() / 3 * 3];
        if idx.iter().any(|&i| i as usize >= pos.len()) {
            return Err("The file refers to vertices that don't exist".into());
        }
        let base = self.pos.len() as u32;
        let first = self.idx.len() as u32;
        let computed;
        let nrm: &[V3] = match nrm {
            Some(n) if n.len() == pos.len() => n,
            _ => {
                computed = smooth_normals(pos, idx);
                &computed
            }
        };
        self.pos.extend_from_slice(pos);
        self.nrm.extend_from_slice(nrm);
        match vcol {
            Some(c) if c.len() == pos.len() => {
                self.col.extend(c.iter().map(|c| [c[0] * color[0], c[1] * color[1], c[2] * color[2]]));
            }
            _ => self.col.extend(std::iter::repeat(color).take(pos.len())),
        }
        match uv {
            Some(t) if t.len() == pos.len() => self.uv.extend_from_slice(t),
            _ => self.uv.extend(std::iter::repeat([0.0f32; 2]).take(pos.len())),
        }
        self.idx.extend(idx.iter().map(|i| i + base));
        let count = self.idx.len() as u32 - first;
        match self.parts.last_mut() {
            Some(last) if last.slot == slot && last.first + last.count == first => last.count += count,
            _ => self.parts.push(Part { first, count, slot }),
        }
        Ok(())
    }
}

/// Texture slots found while loading, de-duplicated by name / image index.
#[derive(Default)]
struct Slots {
    list: Vec<Slot>,
    by_key: HashMap<String, usize>,
    unnamed: Option<usize>,
}

const MAX_SLOTS: usize = 64;
/// Textures the model points at are decoded while loading, but only for the first few materials.
const AUTO_DECODE_MAX: usize = 8;

impl Slots {
    fn named(&mut self, key: String, make: impl FnOnce(bool) -> Slot) -> Option<usize> {
        if let Some(&i) = self.by_key.get(&key) {
            return Some(i);
        }
        if self.list.len() >= MAX_SLOTS {
            return None;
        }
        let decode = self.list.len() < AUTO_DECODE_MAX;
        self.list.push(make(decode));
        let i = self.list.len() - 1;
        self.by_key.insert(key, i);
        Some(i)
    }

    /// For meshes that have texture coordinates but name no texture: the user's texture can still go here.
    fn unnamed(&mut self) -> Option<usize> {
        if self.unnamed.is_some() {
            return self.unnamed;
        }
        if self.list.len() >= MAX_SLOTS {
            return None;
        }
        self.list.push(Slot { name: String::new(), image: None });
        self.unnamed = Some(self.list.len() - 1);
        self.unnamed
    }
}

fn smooth_normals(pos: &[V3], idx: &[u32]) -> Vec<V3> {
    let mut n = vec![[0.0f32; 3]; pos.len()];
    for t in idx.chunks_exact(3) {
        let (a, b, c) = (pos[t[0] as usize], pos[t[1] as usize], pos[t[2] as usize]);
        let f = cross(sub(b, a), sub(c, a)); // length = 2 x area, so big faces weigh more
        for &i in t {
            let s = &mut n[i as usize];
            s[0] += f[0];
            s[1] += f[1];
            s[2] += f[2];
        }
    }
    for v in n.iter_mut() {
        *v = normalize_or(*v, [0.0, 1.0, 0.0]);
    }
    n
}

pub struct ModelData {
    mesh: Mesh,
    format: &'static str,
    /// Start with Z as "up" (STL from CAD / 3D printing usually is).
    zup: bool,
    /// Normals are derived per pixel in the shader (STL: hard edges stay hard, no normal buffer needed).
    flat: bool,
    /// No faces: draw the vertices as points (PLY scans / point clouds).
    points: bool,
    /// Textures the model refers to (empty when it has no texture coordinates).
    slots: Vec<Slot>,
}

pub fn load(path: &Path) -> Result<ModelData, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > MAX_FILE_BYTES {
        return Err("This model file is too large to preview".into());
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let mut data = match ext.as_str() {
        "obj" => load_obj(path)?,
        "stl" => load_stl(path)?,
        "ply" => load_ply(path)?,
        "gltf" | "glb" => load_gltf(path)?,
        _ => return Err("Unsupported 3D format".into()),
    };
    if data.mesh.pos.is_empty() {
        return Err("The model has no geometry".into());
    }
    for p in data.mesh.pos.iter_mut() {
        if !(p[0].is_finite() && p[1].is_finite() && p[2].is_finite()) {
            *p = [0.0; 3];
        }
    }
    Ok(data)
}

// ---- OBJ ----

fn load_obj(path: &Path) -> Result<ModelData, String> {
    let options = tobj::LoadOptions {
        triangulate: true,
        single_index: true,
        ignore_points: true,
        ignore_lines: true,
        ..Default::default()
    };
    let (models, materials) = tobj::load_obj(path, &options).map_err(|e| e.to_string())?;
    let materials = materials.unwrap_or_default(); // a missing .mtl just means "no colours"
    let dir = path.parent();
    let mut mesh = Mesh::default();
    let mut slots = Slots::default();
    for model in &models {
        let m = &model.mesh;
        let pos: Vec<V3> = m.positions.chunks_exact(3).map(|p| [p[0], p[1], p[2]]).collect();
        let nrm: Vec<V3> = m.normals.chunks_exact(3).map(|p| [p[0], p[1], p[2]]).collect();
        let material = m.material_id.and_then(|i| materials.get(i));
        let color = material
            .and_then(|mat| mat.diffuse)
            .filter(|d| d[0] + d[1] + d[2] > 0.03) // black "Kd" is almost always an export accident
            .unwrap_or(DEFAULT_COLOR);

        // Texture coordinates (OBJ has v pointing up; we keep image space, so flip v).
        let has_uv = !pos.is_empty() && m.texcoords.len() == pos.len() * 2;
        let uv: Vec<[f32; 2]> = if has_uv { m.texcoords.chunks_exact(2).map(|t| [t[0], 1.0 - t[1]]).collect() } else { Vec::new() };
        let tex_name = material
            .and_then(|mat| mat.diffuse_texture.as_deref())
            .map(clean_tex_name)
            .filter(|n| !n.is_empty());
        let slot = if !has_uv {
            None
        } else {
            match tex_name {
                Some(name) => {
                    let key = name.to_ascii_lowercase();
                    slots.named(key, |decode| {
                        let image = if decode {
                            dir.map(|d| d.join(name.replace('\\', "/"))).and_then(|p| decode_file(&p).ok())
                        } else {
                            None
                        };
                        Slot { name: name.clone(), image }
                    })
                }
                None => slots.unnamed(),
            }
        };
        mesh.add(&pos, (!nrm.is_empty()).then_some(nrm.as_slice()), None, color, has_uv.then_some(uv.as_slice()), slot, &m.indices)?;
    }
    Ok(ModelData { mesh, format: "OBJ", zup: false, flat: false, points: false, slots: slots.list })
}

/// `map_Kd -s 1 1 1 wood.png` -> `wood.png` (MTL options come first, the file name is last).
fn clean_tex_name(s: &str) -> String {
    let s = s.trim();
    if s.starts_with('-') {
        s.split_whitespace().last().unwrap_or("").to_string()
    } else {
        s.to_string()
    }
}

// ---- STL (binary and ASCII) ----

#[derive(Default)]
struct Fx(u64);
impl Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u32(b as u32);
        }
    }
    fn write_u32(&mut self, i: u32) {
        self.0 = (self.0.rotate_left(5) ^ i as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

fn parse_stl(b: &[u8]) -> Result<Vec<V3>, String> {
    let mut verts: Vec<V3> = Vec::new();
    let binary = b.len() >= 84 && {
        let n = u32::from_le_bytes([b[80], b[81], b[82], b[83]]) as u64;
        let need = 84 + n * 50;
        need == b.len() as u64 || (need < b.len() as u64 && !b.starts_with(b"solid"))
    };
    if binary {
        let n = u32::from_le_bytes([b[80], b[81], b[82], b[83]]) as usize;
        let f = |o: usize| f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        verts.reserve(n * 3);
        for i in 0..n {
            let o = 84 + i * 50 + 12; // skip the stored normal, we don't trust it
            for k in 0..3 {
                let p = o + k * 12;
                verts.push([f(p), f(p + 4), f(p + 8)]);
            }
        }
    } else {
        let text = String::from_utf8_lossy(b);
        let mut it = text.split_ascii_whitespace();
        while let Some(t) = it.next() {
            if t.eq_ignore_ascii_case("vertex") {
                let mut c = [0.0f32; 3];
                for k in 0..3 {
                    c[k] = it.next().and_then(|s| s.parse().ok()).ok_or("Broken vertex in STL file")?;
                }
                verts.push(c);
            }
        }
    }
    if verts.is_empty() || verts.len() % 3 != 0 {
        return Err("This isn't a valid STL file".into());
    }
    Ok(verts)
}

fn load_stl(path: &Path) -> Result<ModelData, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let tri_verts = parse_stl(&bytes)?;
    drop(bytes);
    // STL repeats every vertex for each triangle: weld identical ones (about 6x smaller on the GPU).
    let mut map: HashMap<(u32, u32, u32), u32, BuildHasherDefault<Fx>> = HashMap::default();
    let mut pos: Vec<V3> = Vec::new();
    let mut idx: Vec<u32> = Vec::with_capacity(tri_verts.len());
    for v in &tri_verts {
        let key = (v[0].to_bits(), v[1].to_bits(), v[2].to_bits());
        let id = *map.entry(key).or_insert_with(|| {
            pos.push(*v);
            (pos.len() - 1) as u32
        });
        idx.push(id);
    }
    let n = pos.len();
    let mesh = Mesh { pos, nrm: vec![[0.0; 3]; n], col: vec![DEFAULT_COLOR; n], idx, ..Default::default() };
    Ok(ModelData { mesh, format: "STL", zup: true, flat: true, points: false, slots: Vec::new() })
}

// ---- PLY (ascii, binary little/big endian; vertices, optional normals + colours, polygon faces) ----

#[derive(Clone, Copy, PartialEq)]
enum Ty {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
}

impl Ty {
    fn parse(s: &str) -> Option<Ty> {
        Some(match s {
            "char" | "int8" => Ty::I8,
            "uchar" | "uint8" => Ty::U8,
            "short" | "int16" => Ty::I16,
            "ushort" | "uint16" => Ty::U16,
            "int" | "int32" => Ty::I32,
            "uint" | "uint32" => Ty::U32,
            "float" | "float32" => Ty::F32,
            "double" | "float64" => Ty::F64,
            _ => return None,
        })
    }
    fn size(self) -> usize {
        match self {
            Ty::I8 | Ty::U8 => 1,
            Ty::I16 | Ty::U16 => 2,
            Ty::I32 | Ty::U32 | Ty::F32 => 4,
            Ty::F64 => 8,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    X,
    Y,
    Z,
    Nx,
    Ny,
    Nz,
    R,
    G,
    B,
    Face,
    Other,
}

struct Prop {
    ty: Ty,
    list: Option<Ty>,
    role: Role,
}

struct Elem {
    name: String,
    count: usize,
    props: Vec<Prop>,
}

struct Src<'a> {
    ascii: Option<std::str::SplitAsciiWhitespace<'a>>,
    bin: &'a [u8],
    pos: usize,
    be: bool,
}

impl<'a> Src<'a> {
    fn next(&mut self, ty: Ty) -> Result<f64, String> {
        if let Some(it) = self.ascii.as_mut() {
            return it
                .next()
                .and_then(|t| t.parse::<f64>().ok())
                .ok_or_else(|| "Unexpected data in PLY file".to_string());
        }
        let bin = self.bin;
        let n = ty.size();
        let b = bin.get(self.pos..self.pos + n).ok_or("Unexpected end of PLY file")?;
        self.pos += n;
        let be = self.be;
        macro_rules! rd {
            ($t:ty, $n:expr) => {{
                let mut a = [0u8; $n];
                a.copy_from_slice(&b[..$n]);
                (if be { <$t>::from_be_bytes(a) } else { <$t>::from_le_bytes(a) }) as f64
            }};
        }
        Ok(match ty {
            Ty::I8 => b[0] as i8 as f64,
            Ty::U8 => b[0] as f64,
            Ty::I16 => rd!(i16, 2),
            Ty::U16 => rd!(u16, 2),
            Ty::I32 => rd!(i32, 4),
            Ty::U32 => rd!(u32, 4),
            Ty::F32 => rd!(f32, 4),
            Ty::F64 => rd!(f64, 8),
        })
    }
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn load_ply(path: &Path) -> Result<ModelData, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let header_end = find_bytes(&bytes[..bytes.len().min(1 << 16)], b"end_header").ok_or("This isn't a PLY file")?;
    let mut body = header_end + b"end_header".len();
    while body < bytes.len() && bytes[body] != b'\n' {
        body += 1;
    }
    body = (body + 1).min(bytes.len());

    let header = std::str::from_utf8(&bytes[..header_end]).map_err(|_| "Unreadable PLY header")?;
    let mut lines = header.lines();
    if lines.next().map(str::trim) != Some("ply") {
        return Err("This isn't a PLY file".into());
    }
    let mut format = String::new();
    let mut elems: Vec<Elem> = Vec::new();
    for line in lines {
        let t: Vec<&str> = line.split_whitespace().collect();
        match t.first().copied() {
            Some("format") if t.len() >= 2 => format = t[1].to_string(),
            Some("element") if t.len() >= 3 => elems.push(Elem {
                name: t[1].to_string(),
                count: t[2].parse().map_err(|_| "Bad element count in PLY header")?,
                props: Vec::new(),
            }),
            Some("property") => {
                let Some(elem) = elems.last_mut() else { continue };
                let is_list = t.get(1) == Some(&"list");
                let (list, ty, name) = if is_list && t.len() >= 5 {
                    (Ty::parse(t[2]), Ty::parse(t[3]), t[4])
                } else if !is_list && t.len() >= 3 {
                    (None, Ty::parse(t[1]), t[2])
                } else {
                    continue;
                };
                let (Some(ty), true) = (ty, !is_list || list.is_some()) else {
                    return Err("Unsupported property type in PLY".into());
                };
                let role = match (elem.name.as_str(), name) {
                    ("vertex", "x") => Role::X,
                    ("vertex", "y") => Role::Y,
                    ("vertex", "z") => Role::Z,
                    ("vertex", "nx") => Role::Nx,
                    ("vertex", "ny") => Role::Ny,
                    ("vertex", "nz") => Role::Nz,
                    ("vertex", "red") | ("vertex", "r") => Role::R,
                    ("vertex", "green") | ("vertex", "g") => Role::G,
                    ("vertex", "blue") | ("vertex", "b") => Role::B,
                    ("face", "vertex_indices") | ("face", "vertex_index") if is_list => Role::Face,
                    _ => Role::Other,
                };
                elem.props.push(Prop { ty, list, role });
            }
            _ => {}
        }
    }

    let (ascii, be) = match format.as_str() {
        "ascii" => (true, false),
        "binary_little_endian" => (false, false),
        "binary_big_endian" => (false, true),
        _ => return Err("Unsupported PLY format".into()),
    };
    let data = &bytes[body..];
    let text = ascii.then(|| String::from_utf8_lossy(data));
    let mut src = Src { ascii: text.as_deref().map(|t| t.split_ascii_whitespace()), bin: data, pos: 0, be };

    let (mut pos, mut nrm, mut col): (Vec<V3>, Vec<V3>, Vec<V3>) = (Vec::new(), Vec::new(), Vec::new());
    let mut idx: Vec<u32> = Vec::new();
    let (mut has_n, mut has_c) = (false, false);
    let mut poly: Vec<u32> = Vec::new();

    for e in &elems {
        if e.count > 100_000_000 {
            return Err("PLY element count is unreasonably large".into());
        }
        let is_vertex = e.name == "vertex";
        for _ in 0..e.count {
            let mut v = [0.0f64; 9];
            for p in &e.props {
                if let Some(count_ty) = p.list {
                    let n = (src.next(count_ty)?.max(0.0) as usize).min(1_000_000);
                    if p.role == Role::Face {
                        poly.clear();
                        for _ in 0..n {
                            poly.push(src.next(p.ty)? as u32);
                        }
                        for k in 1..n.saturating_sub(1) {
                            idx.extend([poly[0], poly[k], poly[k + 1]]); // fan-triangulate polygons
                        }
                    } else {
                        for _ in 0..n {
                            src.next(p.ty)?;
                        }
                    }
                } else {
                    let val = src.next(p.ty)?;
                    if is_vertex {
                        let unit = match p.ty {
                            Ty::U8 => val / 255.0,
                            Ty::U16 => val / 65535.0,
                            _ => val,
                        };
                        match p.role {
                            Role::X => v[0] = val,
                            Role::Y => v[1] = val,
                            Role::Z => v[2] = val,
                            Role::Nx => {
                                v[3] = val;
                                has_n = true;
                            }
                            Role::Ny => v[4] = val,
                            Role::Nz => v[5] = val,
                            Role::R => {
                                v[6] = unit;
                                has_c = true;
                            }
                            Role::G => v[7] = unit,
                            Role::B => v[8] = unit,
                            _ => {}
                        }
                    }
                }
            }
            if is_vertex {
                pos.push([v[0] as f32, v[1] as f32, v[2] as f32]);
                nrm.push([v[3] as f32, v[4] as f32, v[5] as f32]);
                col.push([v[6] as f32, v[7] as f32, v[8] as f32]);
            }
        }
    }

    let mut mesh = Mesh::default();
    if idx.is_empty() {
        // Point cloud.
        let n = pos.len();
        mesh.nrm = vec![[0.0, 1.0, 0.0]; n];
        mesh.col = if has_c { col } else { vec![DEFAULT_COLOR; n] };
        mesh.pos = pos;
        return Ok(ModelData { mesh, format: "PLY", zup: false, flat: false, points: true, slots: Vec::new() });
    }
    mesh.add(
        &pos,
        has_n.then_some(nrm.as_slice()),
        has_c.then_some(col.as_slice()),
        if has_c { [1.0; 3] } else { DEFAULT_COLOR },
        None,
        None,
        &idx,
    )?;
    Ok(ModelData { mesh, format: "PLY", zup: false, flat: false, points: false, slots: Vec::new() })
}

// ---- glTF / GLB (geometry + base colour only; textures are skipped to stay light) ----

fn lin_to_srgb(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.003_130_8 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

fn flatten(m: [[f32; 4]; 4]) -> M4 {
    let mut r = [0.0; 16];
    for c in 0..4 {
        for row in 0..4 {
            r[c * 4 + row] = m[c][row];
        }
    }
    r
}

struct GltfCtx<'a> {
    buffers: &'a [gltf::buffer::Data],
    base: Option<&'a Path>,
    slots: Slots,
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = (match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => return None,
        }) as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// The glTF image behind a base-colour texture: embedded (GLB / buffer view), a `data:` URI or a file next to the model.
fn gltf_slot(img: &gltf::Image, base: Option<&Path>, buffers: &[gltf::buffer::Data], decode: bool) -> Slot {
    use gltf::image::Source;
    let (name, image) = match img.source() {
        Source::View { view, .. } => {
            let image = if decode {
                let start = view.offset();
                buffers
                    .get(view.buffer().index())
                    .and_then(|b| b.get(start..start + view.length()))
                    .and_then(|bytes| decode_bytes(bytes).ok())
            } else {
                None
            };
            (img.name().unwrap_or("").to_string(), image)
        }
        Source::Uri { uri, .. } => {
            if let Some(rest) = uri.strip_prefix("data:") {
                let image = if decode {
                    rest.split_once(',')
                        .filter(|(head, _)| head.contains("base64"))
                        .and_then(|(_, data)| base64_decode(data))
                        .and_then(|bytes| decode_bytes(&bytes).ok())
                } else {
                    None
                };
                (img.name().unwrap_or("").to_string(), image)
            } else {
                let decoded = percent_decode(uri);
                let image = if decode { base.and_then(|d| decode_file(&d.join(&decoded)).ok()) } else { None };
                (decoded, image)
            }
        }
    };
    Slot { name, image }
}

fn add_gltf_mesh(gm: &gltf::Mesh, m: &M4, ctx: &mut GltfCtx, mesh: &mut Mesh) -> Result<(), String> {
    for prim in gm.primitives() {
        if prim.mode() != gltf::mesh::Mode::Triangles {
            continue;
        }
        let buffers = ctx.buffers;
        let reader = prim.reader(|b| Some(&buffers[b.index()]));
        let Some(positions) = reader.read_positions() else { continue };
        let pos: Vec<V3> = positions.map(|p| transform_point(m, p)).collect();
        let nrm: Option<Vec<V3>> = reader.read_normals().map(|n| n.map(|v| transform_dir(m, v)).collect());
        let idx: Vec<u32> = match reader.read_indices() {
            Some(i) => i.into_u32().collect(),
            None => (0..pos.len() as u32).collect(),
        };
        let vcol: Option<Vec<V3>> = reader.read_colors(0).map(|c| {
            c.into_rgb_f32()
                .map(|c| [lin_to_srgb(c[0]), lin_to_srgb(c[1]), lin_to_srgb(c[2])])
                .collect()
        });
        let material = prim.material();
        let pbr = material.pbr_metallic_roughness();
        let f = pbr.base_color_factor();
        // 0.92: a textured model usually has a pure-white factor; that would glare without its texture.
        let color = [lin_to_srgb(f[0]) * 0.92, lin_to_srgb(f[1]) * 0.92, lin_to_srgb(f[2]) * 0.92];

        // Base-colour texture (only this map is used).
        let info = pbr.base_color_texture();
        let set = info.as_ref().map(|i| i.tex_coord()).unwrap_or(0);
        let uv: Option<Vec<[f32; 2]>> = reader.read_tex_coords(set).map(|t| t.into_f32().collect());
        let slot = if uv.is_none() {
            None
        } else {
            match &info {
                Some(i) => {
                    let img = i.texture().source();
                    let (base, bufs) = (ctx.base, ctx.buffers);
                    ctx.slots.named(format!("img{}", img.index()), |decode| gltf_slot(&img, base, bufs, decode))
                }
                None => ctx.slots.unnamed(),
            }
        };
        mesh.add(&pos, nrm.as_deref(), vcol.as_deref(), color, uv.as_deref(), slot, &idx)?;
    }
    Ok(())
}

fn visit(node: gltf::Node, parent: &M4, ctx: &mut GltfCtx, mesh: &mut Mesh, depth: u32) -> Result<(), String> {
    let m = mul(parent, &flatten(node.transform().matrix()));
    if let Some(gm) = node.mesh() {
        add_gltf_mesh(&gm, &m, ctx, mesh)?;
    }
    if depth < 64 {
        for child in node.children() {
            visit(child, &m, ctx, mesh, depth + 1)?;
        }
    }
    Ok(())
}

fn load_gltf(path: &Path) -> Result<ModelData, String> {
    let gltf::Gltf { document, blob } = gltf::Gltf::open(path).map_err(|e| e.to_string())?;
    let buffers = gltf::import_buffers(&document, path.parent(), blob).map_err(|e| e.to_string())?;
    let mut mesh = Mesh::default();
    let mut ctx = GltfCtx { buffers: &buffers, base: path.parent(), slots: Slots::default() };
    let id = identity();
    match document.default_scene().or_else(|| document.scenes().next()) {
        Some(scene) => {
            for node in scene.nodes() {
                visit(node, &id, &mut ctx, &mut mesh, 0)?;
            }
        }
        None => {
            for gm in document.meshes() {
                add_gltf_mesh(&gm, &id, &mut ctx, &mut mesh)?;
            }
        }
    }
    if mesh.pos.is_empty() {
        return Err("No triangle meshes found (Draco / meshopt-compressed glTF isn't supported)".into());
    }
    Ok(ModelData { mesh, format: "glTF", zup: false, flat: false, points: false, slots: ctx.slots.list })
}

// ------------------------------------------------------------------------------------------
// Textures: decoding, matching a texture folder / files to the model's materials
// ------------------------------------------------------------------------------------------

pub const TEX_EXTS: &[&str] = &["png", "jpg", "jpeg", "tga", "bmp", "tif", "tiff", "webp", "gif"];
/// Textures are shrunk to fit this (GPU memory and upload time; plenty for a preview).
const TEX_MAX: u32 = 2048;

fn finish_image(img: image::DynamicImage) -> ImageData {
    let img = if img.width() > TEX_MAX || img.height() > TEX_MAX {
        img.resize(TEX_MAX, TEX_MAX, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    ImageData { w: rgba.width(), h: rgba.height(), rgba: rgba.into_raw() }
}

fn open_reader(path: &Path) -> Result<image::ImageReader<std::io::BufReader<std::fs::File>>, String> {
    image::ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())
}

fn decode_file(path: &Path) -> Result<ImageData, String> {
    let (w, h) = open_reader(path)?.into_dimensions().map_err(|e| e.to_string())?;
    if w as u64 * h as u64 > 80_000_000 {
        return Err("Texture is too large".into());
    }
    let img = open_reader(path)?.decode().map_err(|e| e.to_string())?;
    Ok(finish_image(img))
}

fn decode_bytes(bytes: &[u8]) -> Result<ImageData, String> {
    let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    Ok(finish_image(img))
}

fn is_texture_file(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map_or(false, |e| TEX_EXTS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

fn walk_textures(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    if depth > 4 || out.len() > 5000 {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else { return };
    for entry in read.flatten() {
        let p = entry.path();
        if p.is_dir() {
            walk_textures(&p, depth + 1, out);
        } else if is_texture_file(&p) {
            out.push(p);
        }
    }
}

/// Files, plus everything image-like inside folders (texture packs often have sub-folders).
fn collect_pool(inputs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in inputs {
        if p.is_dir() {
            walk_textures(p, 0, &mut out);
        } else if is_texture_file(p) {
            out.push(p.clone());
        }
    }
    out.sort();
    out.dedup();
    out.truncate(5000);
    out
}

/// Lower-case file name without any directory (works for both / and \ paths).
fn norm_name(s: &str) -> String {
    s.rsplit(|c| c == '/' || c == '\\').next().unwrap_or(s).to_ascii_lowercase()
}

fn stem_of(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

/// The picked image a material refers to: same file name, else same name with another extension (.tga vs .png).
fn resolve<'a>(name: &str, pool: &'a [PathBuf]) -> Option<&'a PathBuf> {
    let want = norm_name(name);
    let file = |p: &PathBuf| p.file_name().and_then(|n| n.to_str()).map(|n| n.to_ascii_lowercase()).unwrap_or_default();
    pool.iter().find(|p| file(p) == want).or_else(|| {
        let want_stem = stem_of(&want).to_string();
        pool.iter().find(|p| {
            let f = file(p);
            stem_of(&f) == want_stem.as_str()
        })
    })
}

/// When nothing matches by name: one picked file is used as is; in a pack, prefer albedo / diffuse / colour
/// images and never use normal, roughness, metal, AO, height... maps as a colour. The map type is read from the
/// end of the name (`Metal_Plate_Diffuse.png` is a diffuse map, `Metal_Plate_Normal.png` a normal map).
fn default_pick(pool: &[PathBuf]) -> Option<&PathBuf> {
    if pool.len() == 1 {
        return pool.first();
    }
    const SKIP: &[&str] = &[
        "normal", "normalgl", "normaldx", "nrm", "nor", "norm", "n", "rough", "roughness", "metal", "metallic", "metalness",
        "spec", "specular", "gloss", "glossiness", "bump", "height", "disp", "displacement", "ao", "occlusion",
        "ambientocclusion", "emissive", "emission", "emit", "opacity", "alpha", "mask", "orm", "arm", "rma",
    ];
    const LIKE: &[&str] = &["basecolor", "base_color", "base-color", "albedo", "diffuse", "color", "colour"];
    let mut best: Option<(u32, &PathBuf)> = None;
    for p in pool {
        let Some(file) = p.file_name().and_then(|n| n.to_str()) else { continue };
        let file = file.to_ascii_lowercase();
        let stem = stem_of(&file);
        let last = stem.rsplit(|c: char| c == '_' || c == '-' || c == '.' || c == ' ').next().unwrap_or(stem);
        if SKIP.contains(&last) {
            continue;
        }
        let score = if LIKE.iter().any(|k| stem.contains(k)) { 2 } else { 1 };
        if best.map_or(true, |(b, _)| score > b) {
            best = Some((score, p));
        }
    }
    best.map(|(_, p)| p)
}

enum TexMsg {
    Slot(usize, ImageData),
    Done { note: String },
}

enum TexOp {
    Set(usize, ImageData),
    Clear,
}

/// Worker thread: match the picked files / folder to the model's texture slots, decode, send them back.
fn apply_pool(inputs: Vec<PathBuf>, slots: Vec<(String, bool)>, tx: Sender<TexMsg>, ctx: egui::Context) {
    let pool = collect_pool(&inputs);
    let mut cache: HashMap<PathBuf, ImageData> = HashMap::new();
    let (mut applied, mut failed) = (0usize, 0usize);
    for (i, (name, has_image)) in slots.iter().enumerate() {
        let found = if name.is_empty() { None } else { resolve(name, &pool) };
        // A material whose texture the model already carries keeps it unless a picked file has the same name.
        let Some(path) = found.or_else(|| if *has_image { None } else { default_pick(&pool) }) else { continue };
        let image = match cache.get(path) {
            Some(img) => Some(img.clone()),
            None => match decode_file(path) {
                Ok(img) => {
                    cache.insert(path.clone(), img.clone());
                    Some(img)
                }
                Err(_) => None,
            },
        };
        match image {
            Some(img) => {
                applied += 1;
                if tx.send(TexMsg::Slot(i, img)).is_err() {
                    return; // the view moved on
                }
                ctx.request_repaint();
            }
            None => failed += 1,
        }
    }
    let note = if pool.is_empty() {
        "No image files found in that selection.".to_string()
    } else if applied == 0 {
        format!("None of the {} picked image(s) fits this model's materials.", pool.len())
    } else {
        let mut n = format!("Applied {applied} of {} texture slot(s) from {} image(s).", slots.len(), pool.len());
        if failed > 0 {
            n.push_str(&format!(" {failed} couldn't be read."));
        }
        n
    };
    let _ = tx.send(TexMsg::Done { note });
    ctx.request_repaint();
}

enum Pick {
    Files(Vec<PathBuf>),
    Folder(PathBuf),
}

// ------------------------------------------------------------------------------------------
// GPU side
// ------------------------------------------------------------------------------------------

/// Everything the paint callback needs for one frame (plain data, so the callback is `Send + Sync`).
#[derive(Clone, Copy)]
struct Draw {
    mvp: M4,
    mv: M4,
    grid_mvp: M4,
    grid_mv: M4,
    /// 0 shaded, 1 shaded + wire overlay, 2 wire only
    wire: u8,
    grid: bool,
    flat: bool,
}

const VERTEX_SHADER: &str = r#"
IN vec3 a_pos;
IN vec3 a_nrm;
IN vec3 a_col;
IN vec2 a_uv;
uniform mat4 u_mvp;
uniform mat4 u_mv;
VOUT vec3 v_n;
VOUT vec3 v_p;
VOUT vec3 v_col;
VOUT vec2 v_uv;
void main() {
    v_n = mat3(u_mv) * a_nrm;
    v_p = (u_mv * vec4(a_pos, 1.0)).xyz;
    v_col = a_col;
    v_uv = a_uv;
    gl_Position = u_mvp * vec4(a_pos, 1.0);
    gl_PointSize = 3.0;
}
"#;

const FRAGMENT_SHADER: &str = r#"
#ifdef GL_ES
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
#endif
FIN vec3 v_n;
FIN vec3 v_p;
FIN vec3 v_col;
FIN vec2 v_uv;
uniform float u_mode;   // 0 lit, 1 vertex colour as is (grid, points), 2 flat colour (wireframe)
uniform float u_flat;   // 1: derive the normal from the screen-space slope of the surface
uniform float u_use_tex; // 1: the colour comes from u_tex instead of the vertex colour
uniform sampler2D u_tex;
uniform vec3 u_color;
void main() {
    if (u_mode > 1.5) { FRAG = vec4(u_color, 1.0); return; }
    if (u_mode > 0.5) { FRAG = vec4(v_col, 1.0); return; }
    vec3 base = v_col;
    if (u_use_tex > 0.5) {
        vec4 t = TEXTURE2D(u_tex, v_uv);
        if (t.a < 0.5) discard;   // cut-outs (leaves, fences)
        base = t.rgb;
    }
    vec3 n = (u_flat > 0.5) ? normalize(cross(dFdx(v_p), dFdy(v_p))) : normalize(v_n);
    if (n.z < 0.0) n = -n;   // two-sided: some files have flipped faces
    vec3 l1 = normalize(vec3(-0.35, 0.55, 0.75));
    vec3 l2 = normalize(vec3(0.60, -0.20, 0.40));
    float d1 = max(dot(n, l1), 0.0);
    float d2 = max(dot(n, l2), 0.0);
    vec3 e = normalize(-v_p);
    float s = pow(max(dot(n, normalize(l1 + e)), 0.0), 40.0);
    float sky = 0.5 + 0.5 * n.y;
    vec3 light = vec3(0.20 + 0.12 * sky) + 0.60 * d1 + 0.18 * d2;
    FRAG = vec4(base * light + vec3(0.12 * s), 1.0);
}
"#;

fn bytes_of<T: Copy>(v: &[T]) -> &[u8] {
    // Plain-old-data (f32 / u32) only; GL wants native-endian bytes.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

/// Floats per vertex: position 3, normal 3, colour 3, texture coordinate 2.
const FLOATS: usize = 11;
const STRIDE: i32 = (FLOATS * 4) as i32;

fn grid_vertices() -> Vec<f32> {
    let half = 8i32;
    let mut v: Vec<f32> = Vec::new();
    let mut line = |a: V3, b: V3, c: V3| {
        for p in [a, b] {
            v.extend_from_slice(&p);
            v.extend_from_slice(&[0.0, 1.0, 0.0]);
            v.extend_from_slice(&c);
            v.extend_from_slice(&[0.0, 0.0]);
        }
    };
    for i in -half..=half {
        let t = i as f32 / half as f32;
        let c = if i == 0 { [0.42, 0.45, 0.55] } else { [0.24, 0.26, 0.32] };
        line([t, 0.0, -1.0], [t, 0.0, 1.0], c);
        line([-1.0, 0.0, t], [1.0, 0.0, t], c);
    }
    v
}

/// The GL context handle, so GL objects can free themselves on drop. Only ever touched on the UI thread
/// (paint callbacks and `update` both run there), which is what makes this `Send` wrapper sound.
struct GlHandle(Arc<glow::Context>);
unsafe impl Send for GlHandle {}

struct GlRes {
    gl: GlHandle,
    program: glow::Program,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    ibo: glow::Buffer,
    n_idx: i32,
    n_vert: i32,
    grid_first: i32,
    grid_count: i32,
    parts: Vec<Part>,
    /// One GL texture per texture slot (None: draw with plain colours).
    tex: Vec<Option<glow::Texture>>,
    u_mvp: Option<glow::UniformLocation>,
    u_mv: Option<glow::UniformLocation>,
    u_mode: Option<glow::UniformLocation>,
    u_flat: Option<glow::UniformLocation>,
    u_color: Option<glow::UniformLocation>,
    u_use_tex: Option<glow::UniformLocation>,
    u_tex: Option<glow::UniformLocation>,
}

unsafe fn compile(gl: &glow::Context, kind: u32, source: &str) -> Result<glow::Shader, String> {
    let shader = gl.create_shader(kind)?;
    gl.shader_source(shader, source);
    gl.compile_shader(shader);
    if !gl.get_shader_compile_status(shader) {
        let log = gl.get_shader_info_log(shader);
        gl.delete_shader(shader);
        return Err(format!("Shader error: {log}"));
    }
    Ok(shader)
}

impl GlRes {
    fn new(gl: &Arc<glow::Context>, mesh: &Mesh, points: bool) -> Result<Self, String> {
        let v = gl.version();
        let has_vao = v.major >= 3
            || gl.supported_extensions().contains("GL_ARB_vertex_array_object")
            || gl.supported_extensions().contains("GL_OES_vertex_array_object");
        if !has_vao {
            return Err("3D preview needs OpenGL 3.0 or newer, and this graphics driver reports an older version.".into());
        }
        // GLSL flavour: desktop GL 3.1+ -> 140, 3.0 -> 130, GL ES 3 -> 300 es, otherwise the old attribute/varying style.
        let (decl, new_style) = if v.is_embedded {
            if v.major >= 3 { ("#version 300 es\n", true) } else { ("#version 100\n", false) }
        } else if v.major > 3 || (v.major == 3 && v.minor >= 1) {
            ("#version 140\n", true)
        } else if v.major == 3 {
            ("#version 130\n", true)
        } else {
            ("#version 120\n", false)
        };
        let (vs_pre, fs_pre) = if new_style {
            (
                "#define IN in\n#define VOUT out\n",
                "#define FIN in\n#define TEXTURE2D texture\nout vec4 f_color;\n#define FRAG f_color\n",
            )
        } else {
            (
                "#define IN attribute\n#define VOUT varying\n",
                "#define FIN varying\n#define TEXTURE2D texture2D\n#define FRAG gl_FragColor\n",
            )
        };

        unsafe {
            let vs = compile(gl, glow::VERTEX_SHADER, &format!("{decl}{vs_pre}{VERTEX_SHADER}"))?;
            let fs = match compile(gl, glow::FRAGMENT_SHADER, &format!("{decl}{fs_pre}{FRAGMENT_SHADER}")) {
                Ok(fs) => fs,
                Err(e) => {
                    gl.delete_shader(vs);
                    return Err(e);
                }
            };
            let program = gl.create_program()?;
            gl.attach_shader(program, vs);
            gl.attach_shader(program, fs);
            gl.bind_attrib_location(program, 0, "a_pos");
            gl.bind_attrib_location(program, 1, "a_nrm");
            gl.bind_attrib_location(program, 2, "a_col");
            gl.bind_attrib_location(program, 3, "a_uv");
            gl.link_program(program);
            let linked = gl.get_program_link_status(program);
            gl.detach_shader(program, vs);
            gl.detach_shader(program, fs);
            gl.delete_shader(vs);
            gl.delete_shader(fs);
            if !linked {
                let log = gl.get_program_info_log(program);
                gl.delete_program(program);
                return Err(format!("Shader link error: {log}"));
            }

            // One vertex buffer: the mesh first, the floor grid lines after it.
            let n = mesh.pos.len();
            let grid = grid_vertices();
            let mut data: Vec<f32> = Vec::with_capacity(n * FLOATS + grid.len());
            for i in 0..n {
                data.extend_from_slice(&mesh.pos[i]);
                data.extend_from_slice(&mesh.nrm[i]);
                data.extend_from_slice(&mesh.col[i]);
                data.extend_from_slice(&mesh.uv.get(i).copied().unwrap_or([0.0, 0.0]));
            }
            data.extend_from_slice(&grid);

            let vao = gl.create_vertex_array()?;
            gl.bind_vertex_array(Some(vao));
            let vbo = gl.create_buffer()?;
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes_of(&data), glow::STATIC_DRAW);
            for (loc, size, offset) in [(0u32, 3i32, 0i32), (1, 3, 12), (2, 3, 24), (3, 2, 36)] {
                gl.enable_vertex_attrib_array(loc);
                gl.vertex_attrib_pointer_f32(loc, size, glow::FLOAT, false, STRIDE, offset);
            }
            let ibo = gl.create_buffer()?;
            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(ibo));
            if !points {
                gl.buffer_data_u8_slice(glow::ELEMENT_ARRAY_BUFFER, bytes_of(&mesh.idx), glow::STATIC_DRAW);
            }
            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);

            let mut parts = mesh.parts.clone();
            if parts.is_empty() {
                parts.push(Part { first: 0, count: mesh.idx.len() as u32, slot: None });
            }

            Ok(Self {
                gl: GlHandle(gl.clone()),
                program,
                vao,
                vbo,
                ibo,
                n_idx: if points { 0 } else { mesh.idx.len() as i32 },
                n_vert: n as i32,
                grid_first: n as i32,
                grid_count: (grid.len() / FLOATS) as i32,
                parts,
                tex: Vec::new(),
                u_mvp: gl.get_uniform_location(program, "u_mvp"),
                u_mv: gl.get_uniform_location(program, "u_mv"),
                u_mode: gl.get_uniform_location(program, "u_mode"),
                u_flat: gl.get_uniform_location(program, "u_flat"),
                u_color: gl.get_uniform_location(program, "u_color"),
                u_use_tex: gl.get_uniform_location(program, "u_use_tex"),
                u_tex: gl.get_uniform_location(program, "u_tex"),
            })
        }
    }

    unsafe fn apply(&mut self, op: TexOp) {
        let gl = &*self.gl.0;
        match op {
            TexOp::Clear => {
                for t in self.tex.drain(..).flatten() {
                    gl.delete_texture(t);
                }
            }
            TexOp::Set(slot, img) => {
                if slot >= MAX_SLOTS {
                    return;
                }
                if self.tex.len() <= slot {
                    self.tex.resize(slot + 1, None);
                }
                if let Some(old) = self.tex[slot].take() {
                    gl.delete_texture(old);
                }
                let Ok(t) = gl.create_texture() else { return };
                gl.active_texture(glow::TEXTURE0);
                gl.bind_texture(glow::TEXTURE_2D, Some(t));
                gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    img.w as i32,
                    img.h as i32,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    Some(img.rgba.as_slice()),
                );
                gl.generate_mipmap(glow::TEXTURE_2D);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR_MIPMAP_LINEAR as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::REPEAT as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::REPEAT as i32);
                gl.bind_texture(glow::TEXTURE_2D, None);
                self.tex[slot] = Some(t);
            }
        }
    }

    unsafe fn draw(&self, d: &Draw) {
        let gl = &*self.gl.0;
        let embedded = gl.version().is_embedded;
        gl.disable(glow::BLEND);
        gl.disable(glow::CULL_FACE);
        gl.enable(glow::DEPTH_TEST);
        gl.depth_func(glow::LEQUAL);
        gl.depth_mask(true);
        gl.clear(glow::DEPTH_BUFFER_BIT); // limited to the preview area by egui's scissor rectangle
        if !embedded {
            // Our colours are already display (sRGB) values: write them as they are.
            gl.disable(glow::FRAMEBUFFER_SRGB);
            gl.enable(glow::PROGRAM_POINT_SIZE);
        }
        gl.use_program(Some(self.program));
        gl.bind_vertex_array(Some(self.vao));
        gl.uniform_1_f32(self.u_flat.as_ref(), if d.flat { 1.0 } else { 0.0 });
        gl.uniform_1_f32(self.u_use_tex.as_ref(), 0.0);
        gl.uniform_1_i32(self.u_tex.as_ref(), 0);
        gl.active_texture(glow::TEXTURE0);

        if d.grid {
            gl.uniform_matrix_4_f32_slice(self.u_mvp.as_ref(), false, &d.grid_mvp);
            gl.uniform_matrix_4_f32_slice(self.u_mv.as_ref(), false, &d.grid_mv);
            gl.uniform_1_f32(self.u_mode.as_ref(), 1.0);
            gl.draw_arrays(glow::LINES, self.grid_first, self.grid_count);
        }

        gl.uniform_matrix_4_f32_slice(self.u_mvp.as_ref(), false, &d.mvp);
        gl.uniform_matrix_4_f32_slice(self.u_mv.as_ref(), false, &d.mv);
        if self.n_idx == 0 {
            gl.uniform_1_f32(self.u_mode.as_ref(), 1.0);
            gl.draw_arrays(glow::POINTS, 0, self.n_vert);
        } else {
            let wire = if embedded { 0 } else { d.wire }; // glPolygonMode doesn't exist on GL ES
            if wire != 2 {
                gl.uniform_1_f32(self.u_mode.as_ref(), 0.0);
                if wire == 1 {
                    gl.enable(glow::POLYGON_OFFSET_FILL);
                    gl.polygon_offset(1.0, 1.0);
                }
                // One draw call per run of triangles that share a texture.
                for part in &self.parts {
                    let tex = part.slot.and_then(|s| self.tex.get(s).copied().flatten());
                    match tex {
                        Some(t) => {
                            gl.bind_texture(glow::TEXTURE_2D, Some(t));
                            gl.uniform_1_f32(self.u_use_tex.as_ref(), 1.0);
                        }
                        None => gl.uniform_1_f32(self.u_use_tex.as_ref(), 0.0),
                    }
                    gl.draw_elements(glow::TRIANGLES, part.count as i32, glow::UNSIGNED_INT, (part.first * 4) as i32);
                }
                gl.uniform_1_f32(self.u_use_tex.as_ref(), 0.0);
                gl.bind_texture(glow::TEXTURE_2D, None);
                gl.disable(glow::POLYGON_OFFSET_FILL);
            }
            if wire != 0 {
                gl.polygon_mode(glow::FRONT_AND_BACK, glow::LINE);
                gl.uniform_1_f32(self.u_mode.as_ref(), 2.0);
                let c = if wire == 1 { [0.06, 0.07, 0.10] } else { [0.72, 0.82, 1.0] };
                gl.uniform_3_f32(self.u_color.as_ref(), c[0], c[1], c[2]);
                gl.draw_elements(glow::TRIANGLES, self.n_idx, glow::UNSIGNED_INT, 0);
                gl.polygon_mode(glow::FRONT_AND_BACK, glow::FILL);
            }
        }

        gl.bind_vertex_array(None);
        gl.disable(glow::DEPTH_TEST);
    }
}

impl Drop for GlRes {
    fn drop(&mut self) {
        unsafe {
            let gl = &*self.gl.0;
            for t in self.tex.drain(..).flatten() {
                gl.delete_texture(t);
            }
            gl.delete_program(self.program);
            gl.delete_vertex_array(self.vao);
            gl.delete_buffer(self.vbo);
            gl.delete_buffer(self.ibo);
        }
    }
}

/// Shared between the view (UI thread) and the paint callback. Both run on the UI thread, one after the other.
struct Gpu {
    mesh: Option<Mesh>,
    points: bool,
    res: Option<GlRes>,
    error: Option<String>,
    /// Texture uploads waiting for the next paint (the GL context only exists inside the callback).
    pending: Vec<TexOp>,
}

impl Gpu {
    fn paint(&mut self, gl: &Arc<glow::Context>, d: &Draw) {
        if self.res.is_none() && self.error.is_none() {
            match self.mesh.take() {
                Some(mesh) => match GlRes::new(gl, &mesh, self.points) {
                    Ok(res) => self.res = Some(res),
                    Err(e) => self.error = Some(e),
                },
                None => self.error = Some("The model data is gone".into()),
            }
        }
        if let Some(res) = &mut self.res {
            for op in self.pending.drain(..) {
                unsafe { res.apply(op) };
            }
            unsafe { res.draw(d) };
        }
    }
}

// ------------------------------------------------------------------------------------------
// The view: camera, input, overlay
// ------------------------------------------------------------------------------------------

const FOV_Y: f32 = 45.0 * PI / 180.0;
const NEAR: f32 = 0.01;
const FAR: f32 = 100.0;
const PITCH_MAX: f32 = 1.55;
const GRID_EXTENT: f32 = 1.6;
const MIN_DIST: f32 = 0.05;
const MAX_DIST: f32 = 40.0;

pub struct ModelView {
    gpu: Arc<Mutex<Gpu>>,
    format: &'static str,
    tris: usize,
    verts: usize,
    min: V3,
    max: V3,
    flat: bool,
    zup: bool,
    yaw: f32,
    pitch: f32,
    dist: f32,
    target: V3,
    ortho: bool,
    /// 0 shaded, 1 shaded + wire, 2 wire only
    wire: u8,
    grid: bool,
    spin: bool,
    help: bool,
    fit_pending: bool,
    /// Arrow keys rotate the model. Off when the model sits next to a file list that uses them.
    arrow_keys: bool,
    /// Texture slots (name the model uses, has an image yet).
    slots: Vec<(String, bool)>,
    textured: usize,
    tex_rx: Option<Receiver<TexMsg>>,
    /// A native file dialog is open on its own thread.
    dialog_rx: Option<Receiver<Option<Pick>>>,
    tex_note: String,
}

fn count(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1e6)
    } else if n >= 10_000 {
        format!("{:.1}K", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

fn size_text(v: f32) -> String {
    if v.abs() >= 100.0 {
        format!("{v:.0}")
    } else if v.abs() >= 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    }
}

impl ModelView {
    pub fn new(d: ModelData) -> Self {
        let (mut min, mut max) = ([f32::MAX; 3], [f32::MIN; 3]);
        for p in &d.mesh.pos {
            for k in 0..3 {
                min[k] = min[k].min(p[k]);
                max[k] = max[k].max(p[k]);
            }
        }
        let tris = if d.points { 0 } else { d.mesh.idx.len() / 3 };
        let verts = d.mesh.pos.len();
        // Textures that came with the model are uploaded on the first paint.
        let mut pending = Vec::new();
        let mut slots = Vec::new();
        for (i, slot) in d.slots.into_iter().enumerate() {
            let has_image = slot.image.is_some();
            if let Some(img) = slot.image {
                pending.push(TexOp::Set(i, img));
            }
            slots.push((slot.name, has_image));
        }
        let textured = slots.iter().filter(|s| s.1).count();
        Self {
            gpu: Arc::new(Mutex::new(Gpu { mesh: Some(d.mesh), points: d.points, res: None, error: None, pending })),
            format: d.format,
            tris,
            verts,
            min,
            max,
            flat: d.flat,
            zup: d.zup,
            yaw: 0.6,
            pitch: 0.4,
            dist: 2.8,
            target: [0.0; 3],
            ortho: false,
            wire: 0,
            grid: true,
            spin: false,
            help: false,
            fit_pending: true,
            arrow_keys: true,
            slots,
            textured,
            tex_rx: None,
            dialog_rx: None,
            tex_note: String::new(),
        }
    }

    /// True while a file dialog is open: the preview window must not treat the lost focus as "click away".
    pub fn modal_busy(&self) -> bool {
        self.dialog_rx.is_some()
    }

    fn push_op(&self, op: TexOp) {
        self.gpu.lock().unwrap_or_else(|e| e.into_inner()).pending.push(op);
    }

    fn start_dialog(&mut self, ctx: &egui::Context, folder: bool) {
        if self.dialog_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let pick = if folder {
                rfd::FileDialog::new().set_title("Choose a texture folder").pick_folder().map(Pick::Folder)
            } else {
                rfd::FileDialog::new()
                    .set_title("Choose texture images")
                    .add_filter("Images", TEX_EXTS)
                    .pick_files()
                    .map(Pick::Files)
            };
            let _ = tx.send(pick);
            ctx.request_repaint();
        });
        self.dialog_rx = Some(rx);
    }

    fn start_apply(&mut self, ctx: &egui::Context, inputs: Vec<PathBuf>) {
        if self.slots.is_empty() {
            self.tex_note = "This model has no texture coordinates (UVs), so textures can't be mapped onto it.".into();
            return;
        }
        let slots = self.slots.clone();
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        thread::spawn(move || apply_pool(inputs, slots, tx, ctx));
        self.tex_rx = Some(rx);
        self.tex_note = "Applying textures…".into();
    }

    fn clear_textures(&mut self) {
        self.push_op(TexOp::Clear);
        for s in self.slots.iter_mut() {
            s.1 = false;
        }
        self.textured = 0;
        self.tex_rx = None;
        self.tex_note = "Textures removed.".into();
    }

    /// Collect results from the file dialog and the texture worker (called every frame, cheap).
    fn poll_textures(&mut self, ctx: &egui::Context) {
        let dialog = self.dialog_rx.as_ref().map(|rx| rx.try_recv());
        match dialog {
            Some(Ok(pick)) => {
                self.dialog_rx = None;
                match pick {
                    Some(Pick::Files(files)) => self.start_apply(ctx, files),
                    Some(Pick::Folder(dir)) => self.start_apply(ctx, vec![dir]),
                    None => {}
                }
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.dialog_rx = None;
                self.tex_note = "Couldn't open a file dialog here. Drop image files or a folder onto the window instead.".into();
            }
            _ => {}
        }

        let mut msgs = Vec::new();
        let mut gone = false;
        if let Some(rx) = &self.tex_rx {
            loop {
                match rx.try_recv() {
                    Ok(m) => msgs.push(m),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        gone = true;
                        break;
                    }
                }
            }
        }
        let mut finished = gone;
        for m in msgs {
            match m {
                TexMsg::Slot(i, img) => {
                    if let Some(slot) = self.slots.get_mut(i) {
                        if !slot.1 {
                            slot.1 = true;
                            self.textured += 1;
                        }
                    }
                    self.push_op(TexOp::Set(i, img));
                }
                TexMsg::Done { note } => {
                    self.tex_note = note;
                    finished = true;
                }
            }
        }
        if finished {
            if gone && self.tex_note.starts_with("Applying") {
                self.tex_note = "Reading the textures failed.".into();
            }
            self.tex_rx = None;
        }
    }

    /// Right end of the status bar: the optional "Textures" menu.
    pub fn footer_ui(&mut self, ui: &mut egui::Ui) {
        if self.error().is_some() {
            return;
        }
        let ctx = ui.ctx().clone();
        self.poll_textures(&ctx);
        let busy = self.tex_rx.is_some() || self.dialog_rx.is_some();
        let label = if self.textured > 0 { format!("Textures · {}", self.textured) } else { "Textures".to_string() };
        let can_map = !self.slots.is_empty();
        let reply = ui.menu_button(RichText::new(label).size(13.5), |ui| {
            ui.set_min_width(260.0);
            if ui.add_enabled(can_map && !busy, egui::Button::new("Choose texture files…")).clicked() {
                self.start_dialog(&ctx, false);
                ui.close_menu();
            }
            if ui.add_enabled(can_map && !busy, egui::Button::new("Choose texture folder…")).clicked() {
                self.start_dialog(&ctx, true);
                ui.close_menu();
            }
            if self.textured > 0 && ui.add_enabled(!busy, egui::Button::new("Remove textures")).clicked() {
                self.clear_textures();
                ui.close_menu();
            }
            ui.separator();
            let hint = if !can_map {
                "This model has no texture coordinates (UVs), so textures can't be mapped onto it.".to_string()
            } else if !self.tex_note.is_empty() {
                self.tex_note.clone()
            } else {
                "Optional. You can also drop image files or a folder onto the preview.".to_string()
            };
            ui.add(egui::Label::new(RichText::new(hint).small().weak()).wrap_mode(egui::TextWrapMode::Wrap));
        });
        reply.response.on_hover_text("Optional: use your own texture images or a texture-pack folder");
        if busy {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    pub fn set_embedded(&mut self) {
        self.arrow_keys = false;
    }

    pub fn chips(&self) -> Vec<(&'static str, String)> {
        let geometry = if self.tris > 0 {
            format!("{} tris · {} verts", count(self.tris), count(self.verts))
        } else {
            format!("{} points", count(self.verts))
        };
        let dims = format!(
            "{} × {} × {}",
            size_text(self.max[0] - self.min[0]),
            size_text(self.max[1] - self.min[1]),
            size_text(self.max[2] - self.min[2])
        );
        let mode = ["Shaded", "Wire overlay", "Wireframe"][self.wire as usize % 3];
        let mut chips = vec![
            (icons::CUBE, format!("{} · {geometry}", self.format)),
            (icons::ARROWS_OUT, dims),
            (
                icons::EYE,
                format!("{}-up · {} · {mode}", if self.zup { "Z" } else { "Y" }, if self.ortho { "Ortho" } else { "Perspective" }),
            ),
        ];
        if self.textured > 0 {
            chips.push((icons::IMAGE, format!("{} texture{}", self.textured, if self.textured == 1 { "" } else { "s" })));
        }
        chips
    }

    fn error(&self) -> Option<String> {
        self.gpu.lock().unwrap_or_else(|e| e.into_inner()).error.clone()
    }

    fn ready(&self) -> bool {
        self.gpu.lock().unwrap_or_else(|e| e.into_inner()).res.is_some()
    }

    fn center(&self) -> V3 {
        [
            (self.min[0] + self.max[0]) * 0.5,
            (self.min[1] + self.max[1]) * 0.5,
            (self.min[2] + self.max[2]) * 0.5,
        ]
    }

    fn radius(&self) -> f32 {
        let d = sub(self.max, self.min);
        (dot(d, d).sqrt() * 0.5).max(1e-12)
    }

    /// Height of the model's lowest point in view space (after centring, scaling and the up-axis choice).
    fn floor_y(&self) -> f32 {
        let (c, r) = (self.center(), self.radius());
        let mut lowest = f32::MAX;
        for i in 0..8 {
            let p = [
                if i & 1 == 0 { self.min[0] } else { self.max[0] },
                if i & 2 == 0 { self.min[1] } else { self.max[1] },
                if i & 4 == 0 { self.min[2] } else { self.max[2] },
            ];
            let local = sub(p, c);
            let y = if self.zup { local[2] } else { local[1] };
            lowest = lowest.min(y / r);
        }
        lowest - 0.003 // a hair below, so a flat bottom doesn't flicker against the grid
    }

    fn reset_view(&mut self, aspect: f32) {
        self.yaw = 0.6;
        self.pitch = 0.4;
        self.target = [0.0; 3];
        self.dist = (2.75 / aspect.min(1.0)).clamp(MIN_DIST, MAX_DIST);
    }

    fn zoom(&mut self, factor: f32) {
        self.dist = (self.dist / factor).clamp(MIN_DIST, MAX_DIST);
    }

    fn orbit(&mut self, dx: f32, dy: f32) {
        self.yaw -= dx;
        self.pitch = (self.pitch + dy).clamp(-PITCH_MAX, PITCH_MAX);
    }

    /// `delta`: how far the scene should follow the pointer, in screen pixels.
    fn pan(&mut self, delta: egui::Vec2, viewport_height: f32) {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        let forward = [-cp * sy, -sp, -cp * cy];
        let right = normalize_or(cross(forward, [0.0, 1.0, 0.0]), [1.0, 0.0, 0.0]);
        let up = cross(right, forward);
        let per_pixel = 2.0 * self.dist * (FOV_Y * 0.5).tan() / viewport_height.max(1.0);
        for k in 0..3 {
            self.target[k] += -right[k] * delta.x * per_pixel + up[k] * delta.y * per_pixel;
        }
    }

    fn input(&mut self, ctx: &egui::Context, response: &egui::Response, rect: Rect) {
        let aspect = rect.width() / rect.height();
        let (scroll_y, zoom_delta, dt, shift, ctrl, alt) = ctx.input(|i| {
            (
                i.smooth_scroll_delta.y,
                i.zoom_delta(),
                i.stable_dt.min(0.1),
                i.modifiers.shift,
                i.modifiers.ctrl,
                i.modifiers.alt,
            )
        });

        if response.hovered() {
            if (zoom_delta - 1.0).abs() > 1e-4 {
                self.zoom(zoom_delta);
            }
            if !ctrl && scroll_y != 0.0 {
                self.zoom((scroll_y / 250.0).exp());
            }
            ctx.set_cursor_icon(egui::CursorIcon::Grab);
        }
        if response.dragged() {
            let d = response.drag_delta();
            let (secondary, middle) = ctx.input(|i| (i.pointer.secondary_down(), i.pointer.middle_down()));
            if secondary || middle || shift {
                self.pan(d, rect.height());
            } else {
                self.orbit(d.x * 0.008, d.y * 0.008);
            }
            ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
        }
        if response.double_clicked() {
            self.reset_view(aspect);
        }
        if self.spin {
            self.yaw -= 0.5 * dt;
            ctx.request_repaint();
        }

        if ctx.wants_keyboard_input() {
            return;
        }
        let pressed = |k: Key| ctx.input(|i| i.key_pressed(k));
        let held = |k: Key| ctx.input(|i| i.key_down(k));

        if self.arrow_keys {
            let (l, r, u, d) = (held(Key::ArrowLeft), held(Key::ArrowRight), held(Key::ArrowUp), held(Key::ArrowDown));
            if l || r || u || d {
                let dx = (r as i32 - l as i32) as f32;
                let dy = (d as i32 - u as i32) as f32;
                if shift {
                    self.pan(vec2(dx, dy) * 500.0 * dt, rect.height());
                } else {
                    self.orbit(dx * 1.8 * dt, dy * 1.8 * dt);
                }
                ctx.request_repaint();
            }
        }

        if pressed(Key::Plus) || pressed(Key::Equals) || pressed(Key::PageUp) {
            self.zoom(1.15);
        }
        if pressed(Key::Minus) || pressed(Key::PageDown) {
            self.zoom(1.0 / 1.15);
        }
        if pressed(Key::Num0) || pressed(Key::Home) {
            self.reset_view(aspect);
        }
        // Blender-style views: 1 front, 3 right, 7 top (Ctrl: the opposite side).
        if pressed(Key::Num1) {
            self.yaw = if ctrl { PI } else { 0.0 };
            self.pitch = 0.0;
        }
        if pressed(Key::Num3) {
            self.yaw = if ctrl { -FRAC_PI_2 } else { FRAC_PI_2 };
            self.pitch = 0.0;
        }
        if pressed(Key::Num7) {
            self.pitch = if ctrl { -PITCH_MAX } else { PITCH_MAX };
        }

        if ctrl || alt {
            return;
        }
        if pressed(Key::R) || pressed(Key::F) {
            self.reset_view(aspect);
        }
        if pressed(Key::W) {
            self.wire = (self.wire + 1) % 3;
        }
        if pressed(Key::G) {
            self.grid = !self.grid;
        }
        if pressed(Key::O) {
            self.ortho = !self.ortho;
        }
        if pressed(Key::Z) {
            self.zup = !self.zup;
        }
        if pressed(Key::T) {
            self.spin = !self.spin;
        }
        if pressed(Key::H) {
            self.help = !self.help;
        }
    }

    fn draw_params(&self, aspect: f32) -> Draw {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        let t = self.target;
        let eye = [t[0] + self.dist * cp * sy, t[1] + self.dist * sp, t[2] + self.dist * cp * cy];
        let view = look_at(eye, t, [0.0, 1.0, 0.0]);
        let proj = if self.ortho {
            let half_h = self.dist * (FOV_Y * 0.5).tan();
            ortho(half_h * aspect, half_h, NEAR, FAR)
        } else {
            perspective(FOV_Y, aspect, NEAR, FAR)
        };
        let c = self.center();
        let up = if self.zup { rot_x(-FRAC_PI_2) } else { identity() };
        let model = mul(&scale(1.0 / self.radius()), &mul(&up, &translate([-c[0], -c[1], -c[2]])));
        let mv = mul(&view, &model);
        let grid_model = mul(&translate([0.0, self.floor_y(), 0.0]), &scale(GRID_EXTENT));
        let grid_mv = mul(&view, &grid_model);
        Draw {
            mvp: mul(&proj, &mv),
            mv,
            grid_mvp: mul(&proj, &grid_mv),
            grid_mv,
            wire: self.wire,
            grid: self.grid,
            flat: self.flat,
        }
    }

    fn help_text(&self) -> String {
        let mut s = String::new();
        s.push_str("Mouse\n");
        s.push_str("  Left-drag              Rotate\n");
        s.push_str("  Right / middle drag    Pan   (or Shift + drag)\n");
        s.push_str("  Wheel / pinch          Zoom\n");
        s.push_str("  Double-click           Reset view\n\n");
        s.push_str("Keyboard\n");
        if self.arrow_keys {
            s.push_str("  Arrow keys             Rotate   (Shift: pan)\n");
        }
        s.push_str("  + / -   PgUp / PgDn    Zoom\n");
        s.push_str("  R  F  0  Home          Reset view\n");
        s.push_str("  1  3  7                Front / Right / Top   (Ctrl: opposite)\n");
        s.push_str("  W                      Shaded / wire overlay / wireframe\n");
        s.push_str("  G                      Floor grid\n");
        s.push_str("  O                      Perspective / orthographic\n");
        s.push_str("  Z                      Swap Y-up and Z-up\n");
        s.push_str("  T                      Turntable spin\n");
        s.push_str("  H                      This help");
        s
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        if let Some(message) = self.error() {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.28).max(8.0));
                ui.label(theme::icon_text(icons::WARNING, 40.0).color(Color32::from_rgb(240, 170, 80)));
                ui.add_space(8.0);
                ui.label(RichText::new("Can't show this 3D model").size(17.0));
                ui.add_space(4.0);
                ui.add(egui::Label::new(RichText::new(message).weak()).wrap_mode(egui::TextWrapMode::Wrap));
            });
            return;
        }

        let ctx = ui.ctx().clone();
        let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        if rect.width() < 8.0 || rect.height() < 8.0 {
            return;
        }
        let aspect = rect.width() / rect.height();
        if self.fit_pending {
            self.reset_view(aspect);
            self.fit_pending = false;
        }
        self.input(&ctx, &response, rect);
        self.poll_textures(&ctx);
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if !dropped.is_empty() {
            self.start_apply(&ctx, dropped);
        }
        if !self.ready() {
            ctx.request_repaint(); // the GL objects are created during the first paint
        }

        let draw = self.draw_params(aspect);
        let painter = ui.painter().with_clip_rect(rect);
        painter.rect_filled(rect, 6.0, Color32::from_rgb(33, 35, 43));
        let gpu = self.gpu.clone();
        painter.add(egui::PaintCallback {
            rect,
            callback: Arc::new(egui_glow::CallbackFn::new(move |_info, p| {
                let mut gpu = gpu.lock().unwrap_or_else(|e| e.into_inner());
                gpu.paint(p.gl(), &draw);
            })),
        });

        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            painter.rect_filled(rect, 6.0, Color32::from_black_alpha(150));
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "Drop images or a folder to use as textures",
                FontId::proportional(18.0),
                Color32::WHITE,
            );
        }

        // Overlay: a one-line hint, or the full shortcut list (H).
        if self.help {
            let galley = painter.layout(self.help_text(), FontId::monospace(13.0), Color32::from_gray(225), 640.0);
            let pos = rect.left_top() + vec2(18.0, 18.0);
            let backdrop = Rect::from_min_size(pos, galley.size()).expand(12.0);
            painter.rect_filled(backdrop, 8.0, Color32::from_black_alpha(205));
            painter.galley(pos, galley, Color32::from_gray(225));
        } else {
            painter.text(
                rect.left_bottom() + vec2(12.0, -10.0),
                Align2::LEFT_BOTTOM,
                "Drag: rotate  ·  Right-drag: pan  ·  Wheel: zoom  ·  H: shortcuts",
                FontId::proportional(12.5),
                Color32::from_white_alpha(110),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stl_ascii_one_triangle() {
        let text = b"solid t\n facet normal 0 0 1\n  outer loop\n   vertex 0 0 0\n   vertex 1 0 0\n   vertex 0 1 0\n  endloop\n endfacet\nendsolid t\n";
        let v = parse_stl(text).unwrap();
        assert_eq!(v, vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]);
    }

    #[test]
    fn stl_binary_one_triangle() {
        let mut b = vec![0u8; 80];
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&[0u8; 12]); // normal
        for v in [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
            for c in v {
                b.extend_from_slice(&c.to_le_bytes());
            }
        }
        b.extend_from_slice(&[0u8; 2]); // attribute bytes
        let v = parse_stl(&b).unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(v[1], [1.0, 0.0, 0.0]);
    }

    #[test]
    fn stl_garbage_is_rejected() {
        assert!(parse_stl(b"not a model").is_err());
    }

    #[test]
    fn smooth_normals_of_a_flat_triangle_point_up() {
        let pos = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]];
        let n = smooth_normals(&pos, &[0, 1, 2]);
        assert!((n[0][1] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn mesh_add_rejects_bad_indices() {
        let mut m = Mesh::default();
        assert!(m.add(&[[0.0; 3]; 3], None, None, DEFAULT_COLOR, None, None, &[0, 1, 7]).is_err());
    }

    #[test]
    fn matrix_identity_and_translate() {
        let m = mul(&identity(), &translate([1.0, 2.0, 3.0]));
        assert_eq!(transform_point(&m, [1.0, 1.0, 1.0]), [2.0, 3.0, 4.0]);
    }

    #[test]
    fn texture_names_are_matched_loosely() {
        let pool = vec![PathBuf::from("/t/Wood_Diffuse.PNG"), PathBuf::from("/t/Wood_Normal.png"), PathBuf::from("/t/brick.jpg")];
        assert_eq!(resolve("textures\\brick.jpg", &pool), Some(&pool[2]));
        assert_eq!(resolve("wood_diffuse.tga", &pool), Some(&pool[0])); // other extension, same name
        assert_eq!(resolve("missing.png", &pool), None);
    }

    #[test]
    fn default_pick_prefers_colour_maps() {
        let pool = vec![PathBuf::from("/t/Metal_Plate_Normal.png"), PathBuf::from("/t/Metal_Plate_Diffuse.png")];
        assert_eq!(default_pick(&pool), Some(&pool[1]));
        let only_maps = vec![PathBuf::from("/t/a_normal.png"), PathBuf::from("/t/a_roughness.png")];
        assert_eq!(default_pick(&only_maps), None);
        let single = vec![PathBuf::from("/t/whatever_normal.png")];
        assert_eq!(default_pick(&single), Some(&single[0])); // an explicit single pick is always used
    }

    #[test]
    fn base64_and_percent_decoding() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(percent_decode("my%20tex.png"), "my tex.png");
        assert_eq!(clean_tex_name("-s 1 1 1 wood.png"), "wood.png");
    }

    #[test]
    fn z_up_rotation_maps_z_to_y() {
        let p = transform_point(&rot_x(-FRAC_PI_2), [0.0, 0.0, 1.0]);
        assert!((p[1] - 1.0).abs() < 1e-6 && p[2].abs() < 1e-6);
    }
}
