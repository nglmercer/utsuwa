//! Raw X11 protocol client (no new dependencies): connect, setup with
//! MIT-MAGIC-COOKIE auth, and the requests the backend needs. Kept
//! minimal on purpose — one connection, strictly sequential
//! request/reply, LSB-first (the server answers in our byte order).
//!
//! Wire layout references are inline; every request builder is covered
//! by encoding unit tests, and live tests run against the real server
//! when a display is present (skipped cleanly otherwise).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

#[derive(Debug, thiserror::Error)]
pub enum XError {
    #[error("cannot reach X server: {0}")]
    Connect(String),
    #[error("X setup failed: {0}")]
    Setup(String),
    #[error("X request failed: {0}")]
    Request(String),
    #[error("X error reply {code} on {major}.{minor}")]
    Server { code: u8, major: u8, minor: u16 },
    #[error("protocol: {0}")]
    Protocol(String),
}

pub struct XConn {
    stream: UnixStream,
    pub root: u32,
    pub width_px: u16,
    pub height_px: u16,
    /// Bits per pixel reported by the first usable pixmap format.
    pub bpp: u8,
    /// Server image byte order (0 = LSBFirst).
    pub image_order: u8,
    pub min_keycode: u8,
    pub max_keycode: u8,
    pub root_depth: u8,
    /// RGB masks of the root visual (None for non-TrueColor roots).
    pub root_masks: Option<(u32, u32, u32)>,
    /// Client id allocator state (live capture tests own throwaway
    /// windows through these; production paths don't allocate ids).
    #[cfg(test)]
    id_base: u32,
    #[cfg(test)]
    id_mask: u32,
    #[cfg(test)]
    next_id: u32,
}

fn pad4(mut v: Vec<u8>) -> Vec<u8> {
    while !v.len().is_multiple_of(4) {
        v.push(0);
    }
    v
}

fn read_exact(stream: &mut UnixStream, mut n: usize, what: &str) -> Result<Vec<u8>, XError> {
    let mut out = Vec::with_capacity(n);
    let mut chunk = [0u8; 4096];
    while n > 0 {
        let take = chunk.len().min(n);
        let got = stream
            .read(&mut chunk[..take])
            .map_err(|e| XError::Connect(format!("{what}: {e}")))?;
        if got == 0 {
            return Err(XError::Connect(format!("{what}: eof").to_string()));
        }
        out.extend_from_slice(&chunk[..got]);
        n -= got;
    }
    Ok(out)
}

fn display_number() -> Result<String, XError> {
    let display = std::env::var("DISPLAY").map_err(|_| XError::Connect("no DISPLAY".to_string()))?;
    let after_colon = display.rsplit(':').next().unwrap_or("");
    Ok(after_colon.split('.').next().unwrap_or("0").to_string())
}

fn socket_path() -> Result<String, XError> {
    Ok(format!("/tmp/.X11-unix/X{}", display_number()?))
}

fn xauth_path() -> Option<String> {
    std::env::var("XAUTHORITY")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| format!("{h}/.Xauthority"))
        })
}

/// MIT-MAGIC-COOKIE-1 for display "N" (family Local or wildcard).
fn read_cookie() -> Vec<u8> {
    let path = match xauth_path() {
        Some(p) => p,
        None => return Vec::new(),
    };
    let data = std::fs::read(path).unwrap_or_default();
    let display = format!("unix/:{}", display_number().unwrap_or_else(|_| "0".to_string()));
    let mut o = 0;
    let get = |o: &mut usize, data: &[u8]| -> Option<Vec<u8>> {
        if *o + 2 > data.len() {
            return None;
        }
        let n = u16::from_be_bytes([data[*o], data[*o + 1]]) as usize;
        *o += 2;
        if *o + n > data.len() {
            return None;
        }
        let v = data[*o..*o + n].to_vec();
        *o += n;
        Some(v)
    };
    while o + 2 <= data.len() {
        let family = u16::from_be_bytes([data[o], data[o + 1]]);
        o += 2;
        let addr = get(&mut o, &data);
        let host = get(&mut o, &data);
        let name = get(&mut o, &data);
        let cookie = get(&mut o, &data);
        let (addr, host, name, cookie) = match (addr, host, name, cookie) {
            (Some(a), Some(h), Some(n), Some(c)) => (a, h, n, c),
            _ => break,
        };
        let host_matches = host == display.as_bytes()
            || host == display_number().unwrap_or_default().as_bytes()
            || family == 65535;
        if host_matches && addr.is_empty() && name == b"MIT-MAGIC-COOKIE-1" {
            return cookie;
        }
    }
    Vec::new()
}

impl XConn {
    /// Connect with a timeout. Fails cleanly (for test skipping) when no
    /// display, socket, or cookie path is usable.
    pub fn connect() -> Result<Self, XError> {
        let path = socket_path()?;
        let addr: std::os::unix::net::SocketAddr =
            std::os::unix::net::SocketAddr::from_pathname(&path)
                .map_err(|e| XError::Connect(e.to_string()))?;
        let stream = UnixStream::connect_addr(&addr)
            .map_err(|e| XError::Connect(format!("{path}: {e}")))?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .map_err(|e| XError::Connect(e.to_string()))?;
        let mut conn = Self {
            stream,
            root: 0,
            width_px: 0,
            height_px: 0,
            bpp: 32,
            image_order: 0,
            min_keycode: 8,
            max_keycode: 255,
            root_depth: 24,
            root_masks: None,
            #[cfg(test)]
            id_base: 0,
            #[cfg(test)]
            id_mask: 0,
            #[cfg(test)]
            next_id: 1,
        };
        conn.setup()?;
        Ok(conn)
    }

    fn setup(&mut self) -> Result<(), XError> {
        let cookie = read_cookie();
        let name = b"MIT-MAGIC-COOKIE-1";
        let mut req = vec![0x6c, 0];
        req.extend_from_slice(&11u16.to_le_bytes());
        req.extend_from_slice(&0u16.to_le_bytes());
        req.extend_from_slice(&(name.len() as u16).to_le_bytes());
        req.extend_from_slice(&(cookie.len() as u16).to_le_bytes());
        req.extend_from_slice(&[0, 0]);
        let mut body = pad4(name.to_vec());
        body.extend(pad4(cookie));
        req.extend(body);
        self.stream
            .write_all(&req)
            .map_err(|e| XError::Setup(e.to_string()))?;
        let hdr = read_exact(&mut self.stream, 8, "setup header")?;
        if hdr[0] != 1 {
            return Err(XError::Setup(format!("server refused setup (status {})", hdr[0])));
        }
        let addl = u16::from_le_bytes([hdr[6], hdr[7]]) as usize;
        let body = read_exact(&mut self.stream, addl * 4, "setup body")?;
        #[cfg(test)]
        {
            self.id_base = u32::from_le_bytes(body[4..8].try_into().unwrap());
            self.id_mask = u32::from_le_bytes(body[8..12].try_into().unwrap());
        }
        let vlen = u16::from_le_bytes([body[16], body[17]]) as usize;
        let (nscreens, nfmt) = (body[20], body[21]);
        if nscreens == 0 {
            return Err(XError::Setup("no screens".to_string()));
        }
        let off = 32 + vlen.div_ceil(4) * 4 + nfmt as usize * 8;
        if off + 24 > body.len() {
            return Err(XError::Setup("truncated screen block".to_string()));
        }
        self.root = u32::from_le_bytes(body[off..off + 4].try_into().unwrap());
        self.width_px = u16::from_le_bytes(body[off + 20..off + 22].try_into().unwrap());
        self.height_px = u16::from_le_bytes(body[off + 22..off + 24].try_into().unwrap());
        // First pixmap format with depth >= 24 decides bytes per pixel.
        let fmt_off = 32 + vlen.div_ceil(4) * 4;
        for i in 0..nfmt as usize {
            let f = &body[fmt_off + i * 8..fmt_off + i * 8 + 8];
            if f[0] >= 24 {
                self.bpp = f[1];
                break;
            }
        }
        self.image_order = body[22];
        self.min_keycode = body[26];
        self.max_keycode = body[27];
        // Walk the first screen's depths for the root visual's RGB masks.
        let mut doff = off + 40;
        let ndepths = body[off + 39];
        self.root_depth = body[off + 38];
        for _ in 0..ndepths {
            if doff + 8 > body.len() {
                break;
            }
            let (depth, nvisuals) = (body[doff], body[doff + 2]);
            doff += 8;
            for _ in 0..nvisuals {
                if doff + 24 > body.len() {
                    break;
                }
                let class = body[doff + 4];
                let r = u32::from_le_bytes(body[doff + 8..doff + 12].try_into().unwrap());
                let g = u32::from_le_bytes(body[doff + 12..doff + 16].try_into().unwrap());
                let b = u32::from_le_bytes(body[doff + 16..doff + 20].try_into().unwrap());
                // Class 4 is TrueColor; take the root depth's masks.
                if depth == self.root_depth && class == 4 && self.root_masks.is_none() {
                    self.root_masks = Some((r, g, b));
                }
                doff += 24;
            }
        }
        Ok(())
    }

    fn send(&mut self, opcode: u8, data: u8, payload: &[u8], what: &str) -> Result<(), XError> {
        let n = 1 + payload.len() / 4;
        if !payload.len().is_multiple_of(4) {
            return Err(XError::Protocol(format!("{what}: payload not padded")));
        }
        let mut req = vec![opcode, data];
        req.extend_from_slice(&(n as u16).to_le_bytes());
        req.extend_from_slice(payload);
        self.stream
            .write_all(&req)
            .map_err(|e| XError::Request(format!("{what}: {e}")))?;
        Ok(())
    }

    /// Fire-and-forget requests (WarpPointer, ConfigureWindow, XTEST,
    /// …): the server sends no reply, so reading one would block until
    /// the socket timeout and desync the stream.
    pub fn request_void(&mut self, opcode: u8, data: u8, payload: &[u8]) -> Result<(), XError> {
        self.send(opcode, data, payload, "void request")
    }

    /// One request, one reply. Replies arrive in order; errors surface as
    /// [`XError::Server`] with the failing major opcode.
    pub fn request(&mut self, opcode: u8, data: u8, payload: &[u8]) -> Result<(Vec<u8>, Vec<u8>), XError> {
        self.send(opcode, data, payload, "request")?;
        let hdr = read_exact(&mut self.stream, 32, "reply header")?;
        if hdr[0] == 0 {
            return Err(XError::Server {
                code: hdr[1],
                major: hdr[10],
                minor: u16::from_le_bytes([hdr[8], hdr[9]]),
            });
        }
        if hdr[0] != 1 {
            return Err(XError::Protocol(format!("unexpected reply type {}", hdr[0])));
        }
        let len = u32::from_le_bytes(hdr[4..8].try_into().unwrap()) as usize;
        let extra = read_exact(&mut self.stream, len * 4, "reply body")?;
        Ok((hdr, extra))
    }

    pub fn intern_atom(&mut self, name: &[u8], only_if_exists: bool) -> Result<u32, XError> {
        let mut payload = name.len().to_le_bytes()[..2].to_vec();
        payload.push(u8::from(only_if_exists));
        payload.push(0);
        let payload = pad4([payload, pad4(name.to_vec())].concat());
        let (hdr, _) = self.request(16, 0, &payload)?;
        Ok(u32::from_le_bytes(hdr[8..12].try_into().unwrap()))
    }

    pub fn query_tree(&mut self, window: u32) -> Result<Vec<u32>, XError> {
        // Fixed fields in the header (root@8 parent@12 nchildren@16);
        // the child ids follow as extra data.
        let (hdr, extra) = self.request(15, 0, &window.to_le_bytes())?;
        let n = u16::from_le_bytes(hdr[16..18].try_into().unwrap()) as usize;
        if extra.len() < n * 4 {
            return Err(XError::Protocol("short QueryTree body".to_string()));
        }
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            out.push(u32::from_le_bytes(extra[i * 4..i * 4 + 4].try_into().unwrap()));
        }
        Ok(out)
    }

    /// GetProperty: returns (actual_type, format, value_bytes).
    pub fn get_property(
        &mut self,
        window: u32,
        property: u32,
        req_type: u32,
        long_offset: u32,
        long_length: u32,
    ) -> Result<(u32, u8, Vec<u8>), XError> {
        let mut payload = window.to_le_bytes().to_vec();
        payload.extend_from_slice(&property.to_le_bytes());
        payload.extend_from_slice(&req_type.to_le_bytes());
        payload.extend_from_slice(&long_offset.to_le_bytes());
        payload.extend_from_slice(&long_length.to_le_bytes());
        payload.push(0);
        // Fixed fields in the header: type@8 format@1 nitems@20;
        // the value follows as extra data. Absent properties come back
        // typeless (type 0) with no data.
        let (hdr, extra) = self.request(20, 0, &payload)?;
        let actual = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
        let format = hdr[1];
        let nitems = u32::from_le_bytes(hdr[20..24].try_into().unwrap()) as usize;
        if actual == 0 {
            return Ok((0, 0, Vec::new()));
        }
        let nbytes = match format {
            8 => nitems,
            16 => nitems * 2,
            32 => nitems * 4,
            _ => return Err(XError::Protocol(format!("bad property format {format}"))),
        };
        Ok((actual, format, extra[..nbytes.min(extra.len())].to_vec()))
    }

    pub fn query_extension(&mut self, name: &[u8]) -> Result<(bool, u8), XError> {
        let mut payload = (name.len() as u16).to_le_bytes().to_vec();
        payload.extend_from_slice(&[0, 0]);
        let payload = pad4([payload, pad4(name.to_vec())].concat());
        // Reply carries present/major-opcode in the 32-byte header.
        let (hdr, _) = self.request(98, 0, &payload)?;
        Ok((hdr[8] != 0, hdr[9]))
    }

    pub fn get_geometry(&mut self, drawable: u32) -> Result<(u16, u16, u8), XError> {
        // Header: depth@1 root@8 x@12 y@14 w@16 h@18.
        let (hdr, _) = self.request(14, 0, &drawable.to_le_bytes())?;
        Ok((
            u16::from_le_bytes(hdr[16..18].try_into().unwrap()),
            u16::from_le_bytes(hdr[18..20].try_into().unwrap()),
            hdr[1],
        ))
    }

    /// GetImage ZPixmap of a window; returns (w, h, depth, pixel bytes).
    pub fn get_image(&mut self, window: u32) -> Result<(u16, u16, u8, Vec<u8>), XError> {
        let (w, h, _depth) = self.get_geometry(window)?;
        // plane-mask must fit the root depth or the server Match-errors.
        let mask = match self.root_depth {
            d if d >= 32 => 0xFFFF_FFFFu32,
            d if d >= 24 => 0x00FF_FFFFu32,
            d => (1u32 << d).saturating_sub(1),
        };
        // Format rides in the request's second byte (2 = ZPixmap).
        let mut payload = window.to_le_bytes().to_vec();
        payload.extend_from_slice(&0i16.to_le_bytes());
        payload.extend_from_slice(&0i16.to_le_bytes());
        payload.extend_from_slice(&w.to_le_bytes());
        payload.extend_from_slice(&h.to_le_bytes());
        payload.extend_from_slice(&mask.to_le_bytes());
        // Header: depth@1 visual@8; pixels follow as extra.
        let (hdr, extra) = self.request(73, 2, &payload)?;
        Ok((w, h, hdr[1], extra))
    }

    /// QueryPointer on a window: (same_screen, root_x, root_y).
    /// Toolkit surface (tests + future motion verification).
    #[allow(dead_code)]
    pub fn query_pointer(&mut self, window: u32) -> Result<(bool, i16, i16), XError> {
        // Header: same@1 root@8 child@12 root-x@16 root-y@18.
        let (hdr, _) = self.request(38, 0, &window.to_le_bytes())?;
        Ok((
            hdr[1] != 0,
            i16::from_le_bytes(hdr[16..18].try_into().unwrap()),
            i16::from_le_bytes(hdr[18..20].try_into().unwrap()),
        ))
    }

    /// WarpPointer to absolute root coordinates. dst=None would mean a
    /// *relative* jump, so dst is always the root window here.
    pub fn warp_pointer(&mut self, x: i16, y: i16) -> Result<(), XError> {
        let none: u32 = 0;
        let mut payload = none.to_le_bytes().to_vec(); // src: current
        payload.extend_from_slice(&self.root.to_le_bytes()); // dst: root
        payload.extend_from_slice(&0i16.to_le_bytes());
        payload.extend_from_slice(&0i16.to_le_bytes());
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.extend_from_slice(&x.to_le_bytes());
        payload.extend_from_slice(&y.to_le_bytes());
        self.request_void(41, 0, &payload)
    }

    /// XTEST FakeInput key/button event. `is_press` selects press/release.
    pub fn xtest_fake_key(
        &mut self,
        major: u8,
        keycode: u8,
        is_press: bool,
    ) -> Result<(), XError> {
        let mut payload = vec![if is_press { 2 } else { 3 }, keycode, 0, 0];
        payload.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
        payload.extend_from_slice(&self.root.to_le_bytes());
        payload.extend_from_slice(&[0; 16]);
        self.request_void(major, 2, &payload)
    }

    /// XTEST FakeInput button event.
    pub fn xtest_fake_button(
        &mut self,
        major: u8,
        button: u8,
        is_press: bool,
    ) -> Result<(), XError> {
        let mut payload = vec![if is_press { 4 } else { 5 }, button, 0, 0];
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&self.root.to_le_bytes());
        payload.extend_from_slice(&[0; 16]);
        self.request_void(major, 2, &payload)
    }

    /// Keycode range + keysyms per keycode (for type_text mapping).
    /// keysyms-per-keycode rides at header byte 1; the keysym list
    /// follows as extra data.
    pub fn get_keyboard_mapping(
        &mut self,
        first: u8,
        count: u8,
    ) -> Result<(u8, Vec<u32>), XError> {
        let (hdr, extra) = self.request(101, 0, &[first, count, 0, 0])?;
        let per = hdr[1];
        let mut syms = Vec::new();
        for chunk in extra.chunks(4) {
            if chunk.len() == 4 {
                syms.push(u32::from_le_bytes(chunk.try_into().unwrap()));
            }
        }
        Ok((per, syms))
    }

    /// ConfigureWindow raise (stack-mode Above): the honest "activate".
    pub fn raise(&mut self, window: u32) -> Result<(), XError> {
        // value-mask sibling=0 stack-mode=0(Above): mask=0x0040, values=[0]
        let mut payload = window.to_le_bytes().to_vec();
        payload.extend_from_slice(&0x0040u16.to_le_bytes());
        payload.extend_from_slice(&[0, 0]);
        payload.extend_from_slice(&0u32.to_le_bytes());
        self.request_void(12, 0, &payload)
    }

    /// Allocate a client resource id from the setup base/mask.
    #[cfg(test)]
    pub fn alloc_id(&mut self) -> u32 {
        let id = (self.id_base & !self.id_mask) | (self.next_id & self.id_mask);
        self.next_id = self.next_id.wrapping_add(2);
        id
    }

    /// CreateWindow (InputOutput, CopyFromParent visual) with a solid
    /// background pixel. Returns the caller-allocated id.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub fn create_window(
        &mut self,
        id: u32,
        parent: u32,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
        background: u32,
    ) -> Result<(), XError> {
        let mut payload = id.to_le_bytes().to_vec();
        payload.extend_from_slice(&parent.to_le_bytes());
        payload.extend_from_slice(&x.to_le_bytes());
        payload.extend_from_slice(&y.to_le_bytes());
        payload.extend_from_slice(&w.to_le_bytes());
        payload.extend_from_slice(&h.to_le_bytes());
        payload.extend_from_slice(&0u16.to_le_bytes()); // border
        payload.extend_from_slice(&1u16.to_le_bytes()); // InputOutput
        payload.extend_from_slice(&0u32.to_le_bytes()); // CopyFromParent visual
        payload.extend_from_slice(&0x0002u32.to_le_bytes()); // background-pixel
        payload.extend_from_slice(&background.to_le_bytes());
        // Depth rides in the request's second byte (0 = CopyFromParent).
        self.request_void(1, 0, &payload)
    }

    #[cfg(test)]
    pub fn map_window(&mut self, window: u32) -> Result<(), XError> {
        self.request_void(8, 0, &window.to_le_bytes())
    }

    #[cfg(test)]
    pub fn destroy_window(&mut self, window: u32) -> Result<(), XError> {
        self.request_void(4, 0, &window.to_le_bytes())
    }

    pub fn set_input_focus(&mut self, window: u32) -> Result<(), XError> {
        // revert-to Parent rides in the request's second byte.
        let mut payload = window.to_le_bytes().to_vec();
        payload.extend_from_slice(&0u32.to_le_bytes()); // CurrentTime
        self.request_void(42, 1, &payload)
    }
}
