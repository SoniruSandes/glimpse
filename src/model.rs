//! 3D model preview: OBJ (tobj), glTF / GLB (gltf), STL and PLY (small built-in parsers).
//!
//! Loading happens on a background thread (`load`). Drawing is done on the GPU through an egui_glow paint
//! callback, so there are no extra rendering dependencies and large meshes stay smooth. GL objects are created
//! lazily on the first paint (the callback is the only place that has the GL context) and freed on drop.
//!
//! Controls (see `HELP` for the list shown with H): left-drag orbit, right/middle/Shift-drag pan, wheel zoom,
//! arrow keys, +/-, R reset, 1/3/7 views, W wireframe, G grid, O ortho, Z up-axis, T turntable.

use std::{
    collections::HashMap,
    f32::consts::{FRAC_PI_2, PI},
    hash::{BuildHasherDefault, Hasher},
    path::Path,
    sync::{Arc, Mutex},
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

#[derive(Default)]
pub struct Mesh {
    pos: Vec<V3>,
    nrm: Vec<V3>,
    col: Vec<V3>,
    idx: Vec<u32>,
}

impl Mesh {
    /// Append one triangle batch. Missing normals are computed (smooth). `color` multiplies the vertex colours.
    fn add(&mut self, pos: &[V3], nrm: Option<&[V3]>, vcol: Option<&[V3]>, color: V3, idx: &[u32]) -> Result<(), String> {
        let idx = &idx[..idx.len() / 3 * 3];
        if idx.iter().any(|&i| i as usize >= pos.len()) {
            return Err("The file refers to vertices that don't exist".into());
        }
        let base = self.pos.len() as u32;
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
        self.idx.extend(idx.iter().map(|i| i + base));
        Ok(())
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
    let mut mesh = Mesh::default();
    for model in &models {
        let m = &model.mesh;
        let pos: Vec<V3> = m.positions.chunks_exact(3).map(|p| [p[0], p[1], p[2]]).collect();
        let nrm: Vec<V3> = m.normals.chunks_exact(3).map(|p| [p[0], p[1], p[2]]).collect();
        let color = m
            .material_id
            .and_then(|i| materials.get(i))
            .and_then(|mat| mat.diffuse)
            .filter(|d| d[0] + d[1] + d[2] > 0.03) // black "Kd" is almost always an export accident
            .unwrap_or(DEFAULT_COLOR);
        mesh.add(&pos, (!nrm.is_empty()).then_some(nrm.as_slice()), None, color, &m.indices)?;
    }
    Ok(ModelData { mesh, format: "OBJ", zup: false, flat: false, points: false })
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
    let mesh = Mesh { pos, nrm: vec![[0.0; 3]; n], col: vec![DEFAULT_COLOR; n], idx };
    Ok(ModelData { mesh, format: "STL", zup: true, flat: true, points: false })
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
        return Ok(ModelData { mesh, format: "PLY", zup: false, flat: false, points: true });
    }
    mesh.add(&pos, has_n.then_some(nrm.as_slice()), has_c.then_some(col.as_slice()), if has_c { [1.0; 3] } else { DEFAULT_COLOR }, &idx)?;
    Ok(ModelData { mesh, format: "PLY", zup: false, flat: false, points: false })
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

fn add_gltf_mesh(gm: &gltf::Mesh, m: &M4, buffers: &[gltf::buffer::Data], mesh: &mut Mesh) -> Result<(), String> {
    for prim in gm.primitives() {
        if prim.mode() != gltf::mesh::Mode::Triangles {
            continue;
        }
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
        let f = prim.material().pbr_metallic_roughness().base_color_factor();
        // 0.92: a textured model usually has a pure-white factor; that would glare without its texture.
        let color = [lin_to_srgb(f[0]) * 0.92, lin_to_srgb(f[1]) * 0.92, lin_to_srgb(f[2]) * 0.92];
        mesh.add(&pos, nrm.as_deref(), vcol.as_deref(), color, &idx)?;
    }
    Ok(())
}

fn visit(node: gltf::Node, parent: &M4, buffers: &[gltf::buffer::Data], mesh: &mut Mesh, depth: u32) -> Result<(), String> {
    let m = mul(parent, &flatten(node.transform().matrix()));
    if let Some(gm) = node.mesh() {
        add_gltf_mesh(&gm, &m, buffers, mesh)?;
    }
    if depth < 64 {
        for child in node.children() {
            visit(child, &m, buffers, mesh, depth + 1)?;
        }
    }
    Ok(())
}

fn load_gltf(path: &Path) -> Result<ModelData, String> {
    let gltf::Gltf { document, blob } = gltf::Gltf::open(path).map_err(|e| e.to_string())?;
    let buffers = gltf::import_buffers(&document, path.parent(), blob).map_err(|e| e.to_string())?;
    let mut mesh = Mesh::default();
    let id = identity();
    match document.default_scene().or_else(|| document.scenes().next()) {
        Some(scene) => {
            for node in scene.nodes() {
                visit(node, &id, &buffers, &mut mesh, 0)?;
            }
        }
        None => {
            for gm in document.meshes() {
                add_gltf_mesh(&gm, &id, &buffers, &mut mesh)?;
            }
        }
    }
    if mesh.pos.is_empty() {
        return Err("No triangle meshes found (Draco / meshopt-compressed glTF isn't supported)".into());
    }
    Ok(ModelData { mesh, format: "glTF", zup: false, flat: false, points: false })
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
uniform mat4 u_mvp;
uniform mat4 u_mv;
VOUT vec3 v_n;
VOUT vec3 v_p;
VOUT vec3 v_col;
void main() {
    v_n = mat3(u_mv) * a_nrm;
    v_p = (u_mv * vec4(a_pos, 1.0)).xyz;
    v_col = a_col;
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
uniform float u_mode;   // 0 lit, 1 vertex colour as is (grid, points), 2 flat colour (wireframe)
uniform float u_flat;   // 1: derive the normal from the screen-space slope of the surface
uniform vec3 u_color;
void main() {
    if (u_mode > 1.5) { FRAG = vec4(u_color, 1.0); return; }
    if (u_mode > 0.5) { FRAG = vec4(v_col, 1.0); return; }
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
    FRAG = vec4(v_col * light + vec3(0.12 * s), 1.0);
}
"#;

fn bytes_of<T: Copy>(v: &[T]) -> &[u8] {
    // Plain-old-data (f32 / u32) only; GL wants native-endian bytes.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

fn grid_vertices() -> Vec<f32> {
    let half = 8i32;
    let mut v: Vec<f32> = Vec::new();
    let mut line = |a: V3, b: V3, c: V3| {
        for p in [a, b] {
            v.extend_from_slice(&p);
            v.extend_from_slice(&[0.0, 1.0, 0.0]);
            v.extend_from_slice(&c);
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
    u_mvp: Option<glow::UniformLocation>,
    u_mv: Option<glow::UniformLocation>,
    u_mode: Option<glow::UniformLocation>,
    u_flat: Option<glow::UniformLocation>,
    u_color: Option<glow::UniformLocation>,
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
                "#define FIN in\nout vec4 f_color;\n#define FRAG f_color\n",
            )
        } else {
            (
                "#define IN attribute\n#define VOUT varying\n",
                "#define FIN varying\n#define FRAG gl_FragColor\n",
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
            let mut data: Vec<f32> = Vec::with_capacity(n * 9 + grid.len());
            for i in 0..n {
                data.extend_from_slice(&mesh.pos[i]);
                data.extend_from_slice(&mesh.nrm[i]);
                data.extend_from_slice(&mesh.col[i]);
            }
            data.extend_from_slice(&grid);

            let vao = gl.create_vertex_array()?;
            gl.bind_vertex_array(Some(vao));
            let vbo = gl.create_buffer()?;
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes_of(&data), glow::STATIC_DRAW);
            for (loc, offset) in [(0u32, 0i32), (1, 12), (2, 24)] {
                gl.enable_vertex_attrib_array(loc);
                gl.vertex_attrib_pointer_f32(loc, 3, glow::FLOAT, false, 36, offset);
            }
            let ibo = gl.create_buffer()?;
            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(ibo));
            if !points {
                gl.buffer_data_u8_slice(glow::ELEMENT_ARRAY_BUFFER, bytes_of(&mesh.idx), glow::STATIC_DRAW);
            }
            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);

            Ok(Self {
                gl: GlHandle(gl.clone()),
                program,
                vao,
                vbo,
                ibo,
                n_idx: if points { 0 } else { mesh.idx.len() as i32 },
                n_vert: n as i32,
                grid_first: n as i32,
                grid_count: (grid.len() / 9) as i32,
                u_mvp: gl.get_uniform_location(program, "u_mvp"),
                u_mv: gl.get_uniform_location(program, "u_mv"),
                u_mode: gl.get_uniform_location(program, "u_mode"),
                u_flat: gl.get_uniform_location(program, "u_flat"),
                u_color: gl.get_uniform_location(program, "u_color"),
            })
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
                gl.draw_elements(glow::TRIANGLES, self.n_idx, glow::UNSIGNED_INT, 0);
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
        if let Some(res) = &self.res {
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
        Self {
            gpu: Arc::new(Mutex::new(Gpu { mesh: Some(d.mesh), points: d.points, res: None, error: None })),
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
        vec![
            (icons::CUBE, format!("{} · {geometry}", self.format)),
            (icons::ARROWS_OUT, dims),
            (
                icons::EYE,
                format!("{}-up · {} · {mode}", if self.zup { "Z" } else { "Y" }, if self.ortho { "Ortho" } else { "Perspective" }),
            ),
        ]
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
        assert!(m.add(&[[0.0; 3]; 3], None, None, DEFAULT_COLOR, &[0, 1, 7]).is_err());
    }

    #[test]
    fn matrix_identity_and_translate() {
        let m = mul(&identity(), &translate([1.0, 2.0, 3.0]));
        assert_eq!(transform_point(&m, [1.0, 1.0, 1.0]), [2.0, 3.0, 4.0]);
    }

    #[test]
    fn z_up_rotation_maps_z_to_y() {
        let p = transform_point(&rot_x(-FRAC_PI_2), [0.0, 0.0, 1.0]);
        assert!((p[1] - 1.0).abs() < 1e-6 && p[2].abs() < 1e-6);
    }
}
