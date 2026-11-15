//! Port of `src/screen.c` — the mux's per-pane terminal emulator
//! (cell grid, SGR parser, cursor addressing, scroll regions).
//!
//! Field-for-field API from `thesh.h`'s MuxScreen/MuxCell/MuxColor
//! declarations; behavior is a faithful port of screen.c — the C source
//! is the specification (wrap/deferred-wrap, scroll-region clamping,
//! wide-glyph pad cells, scrollback push rules, CSI param defaults,
//! incremental UTF-8 across feed() chunks, and the exact bytes repaint
//! emits are all copied from the C).

use crate::utf8;
use crate::util;

/// Cell attribute bits (SGR) — from thesh.h.
pub const CA_BOLD: u8 = 0x01;
pub const CA_DIM: u8 = 0x02;
pub const CA_ITALIC: u8 = 0x04;
pub const CA_UL: u8 = 0x08;
pub const CA_REV: u8 = 0x10;
pub const CA_BLINK: u8 = 0x20;
pub const CA_INVIS: u8 = 0x40;
pub const CA_STRIKE: u8 = 0x80;

/// Cell colors: kind 0 = terminal default, 1 = basic SGR (idx holds the
/// original 30..97/40..107 number), 2 = 256-palette, 3 = 24-bit rgb.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MuxColor {
    pub kind: u8,
    pub idx: u8,
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MuxCell {
    pub cp: u32,   // 0 = blank space
    pub attrs: u8, // CA_* bits
    pub pad: u8,   // 1 = 2nd half of a wide glyph
    pub fg: MuxColor,
    pub bg: MuxColor,
}

/// Writeback for child panes (DSR/DA replies) — C: reply fn + ctx.
pub type ReplyFn = Box<dyn FnMut(&[u8])>;

pub struct MuxScreen {
    pub w: i32,
    pub h: i32,             // interior grid (no border)
    pub cells: Vec<MuxCell>, // w*h
    pub alt: Vec<MuxCell>,   // alternate screen if held
    pub cx: i32,
    pub cy: i32,
    pub wrap: i32,    // cursor + deferred wrap
    pub autowrap: i32, // DECAWM (on by default)
    pub attrs: u8,
    pub fg: MuxColor,
    pub bg: MuxColor, // current pen
    pub top: i32,
    pub bot: i32, // scroll region (0-based)
    pub scx: i32,
    pub scy: i32, // saved cursor (DECSC)
    pub sattrs: u8,
    pub sfg: MuxColor,
    pub sbg: MuxColor,
    pub ssaved: i32,
    pub asx: i32,
    pub asy: i32,   // cursor saved by 1048/1049
    pub cur_vis: i32, // DECTCEM (1 = visible)
    pub mouse: i32, // requested mouse level (see thesh.h)
    pub sb: Vec<Vec<MuxCell>>, // scrollback ring rows
    pub sb_ws: Vec<i32>,       // that row's width
    pub sb_head: i32,
    pub sb_count: i32, // next slot / live count
    pub view: i32,     // lines scrolled up (0 = live)
    pub dirty: Vec<u8>, // one flag per row
    pub all_dirty: i32,
    // parser state — persists across feed() chunks
    pub st: i32,                 // PT_* state (screen.c)
    pub csib: [u8; 40],
    pub csin: i32,       // raw CSI collected
    pub cp_p: [i32; 16],
    pub cp_n: i32,       // parsed CSI params
    pub csi_priv: u8,    // '?', '>', '<', '=' or 0
    pub u_buf: [u8; 4],
    pub u_have: i32,
    pub u_need: i32,     // incremental utf-8
    pub last_cp: u32,    // last printed (for REP)
    pub reply: Option<ReplyFn>,
}

impl Default for MuxScreen {
    fn default() -> Self {
        MuxScreen {
            w: 0,
            h: 0,
            cells: Vec::new(),
            alt: Vec::new(),
            cx: 0,
            cy: 0,
            wrap: 0,
            autowrap: 1,
            attrs: 0,
            fg: MuxColor::default(),
            bg: MuxColor::default(),
            top: 0,
            bot: 0,
            scx: 0,
            scy: 0,
            sattrs: 0,
            sfg: MuxColor::default(),
            sbg: MuxColor::default(),
            ssaved: 0,
            asx: 0,
            asy: 0,
            cur_vis: 1,
            mouse: 0,
            sb: Vec::new(),
            sb_ws: Vec::new(),
            sb_head: 0,
            sb_count: 0,
            view: 0,
            dirty: Vec::new(),
            all_dirty: 1,
            st: 0,
            csib: [0; 40],
            csin: 0,
            cp_p: [0; 16],
            cp_n: 0,
            csi_priv: 0,
            u_buf: [0; 4],
            u_have: 0,
            u_need: 0,
            last_cp: 0,
            reply: None,
        }
    }
}

/// thesh.h `MUX_SCROLLBACK` — scrollback ring capacity.
const MUX_SCROLLBACK: usize = 1000;

/* parser states (screen.c: enum { PT_NORM, PT_ESC, PT_CSI, PT_OSC, PT_SKIP, PT_CS }) */
const PT_NORM: i32 = 0;
const PT_ESC: i32 = 1;
const PT_CSI: i32 = 2;
const PT_OSC: i32 = 3;
const PT_SKIP: i32 = 4;
const PT_CS: i32 = 5;

impl MuxScreen {
    /// Port of `mux_screen_init()`.
    ///
    /// DEVIATION: C starts with `memset(s, 0, sizeof *s)`, which would also
    /// clear `reply`; every C call site (mux.c) installs reply *after*
    /// init(), so preserving an already-installed callback here is
    /// observationally identical for the C call order and keeps the Rust
    /// construction order (fields first, init later) from silently losing
    /// DSR/DA writeback.
    pub fn init(&mut self, w: i32, h: i32) {
        let reply = self.reply.take();
        *self = MuxScreen::default();
        self.reply = reply;
        let w = if w < 1 { 1 } else { w };
        let h = if h < 1 { 1 } else { h };
        self.w = w;
        self.h = h;
        self.cells = vec![MuxCell::default(); (w * h) as usize];
        self.top = 0;
        self.bot = h - 1;
        // mark_all(): every row dirty + all_dirty (already 1 from Default)
        self.dirty = vec![1u8; h as usize];
        // autowrap=1, cur_vis=1, st=PT_NORM, pen defaults — from Default,
        // which mirrors the memset-zero + assignments C does here.
    }

    /// Port of `mux_screen_resize()`.
    pub fn resize(&mut self, w: i32, h: i32) {
        let w = if w < 1 { 1 } else { w };
        let h = if h < 1 { 1 } else { h };
        if w == self.w && h == self.h {
            return;
        }
        let (ow, oh) = (self.w, self.h);
        self.cells = shrink_copy(&self.cells, ow, oh, w, h);
        if !self.alt.is_empty() {
            self.alt = shrink_copy(&self.alt, ow, oh, w, h);
        }
        self.w = w;
        self.h = h;
        // a wide pair may have lost its second half at the new right edge
        let ncopy = if ow < w { ow } else { w };
        let rmax = if oh < h { oh } else { h };
        for r in 0..rmax {
            let base = (r * w) as usize;
            fix_row_pairs(&mut self.cells[base..base + w as usize], ncopy);
        }
        if !self.alt.is_empty() {
            for r in 0..rmax {
                let base = (r * w) as usize;
                fix_row_pairs(&mut self.alt[base..base + w as usize], ncopy);
            }
        }
        if self.cx >= w {
            self.cx = w - 1;
        }
        if self.cy >= h {
            self.cy = h - 1;
        }
        self.top = 0;
        self.bot = h - 1; // region resets on resize
        self.wrap = 0;
        self.dirty = vec![1u8; h as usize];
        self.all_dirty = 1;
    }

    /// Port of `mux_screen_feed()`.
    pub fn feed(&mut self, data: &[u8]) {
        let st_in = self.st;
        if util::muxdbg_on() && !data.is_empty() {
            util::muxdbg(&format!(
                "feed {:p} n={} st_in={} cx={} cy={}\n",
                self as *const MuxScreen,
                data.len(),
                st_in,
                self.cx,
                self.cy
            ));
            util::muxdbg_bytes(data);
        }
        let mut i = 0;
        while i < data.len() {
            // finish a multi-byte utf-8 sequence stashed from a prior chunk
            if self.u_need > 0 {
                let b = data[i];
                i += 1;
                // u_have < u_need <= 4 at this point, so the index is in range
                self.u_buf[self.u_have as usize] = b;
                self.u_have += 1;
                if self.u_have >= self.u_need {
                    let (cp, _len, ok) = utf8::decode(&self.u_buf);
                    if ok {
                        self.put_cp(cp);
                    }
                    self.u_have = 0;
                    self.u_need = 0;
                }
                continue;
            }
            let c = data[i];
            if c >= 0x80 && self.st == PT_NORM {
                // lead byte
                let need = if c & 0xf8 == 0xf0 {
                    3
                } else if c & 0xf0 == 0xe0 {
                    2
                } else if c & 0xe0 == 0xc0 {
                    1
                } else {
                    0
                };
                if need > 0 {
                    self.u_buf[0] = c;
                    self.u_have = 1;
                    self.u_need = 1 + need;
                    i += 1;
                    continue;
                }
                self.put_cp(c as u32); // invalid: raw byte
                i += 1;
                continue;
            }
            self.feed_byte(c);
            i += 1;
        }
        if util::muxdbg_on() && !data.is_empty() {
            util::muxdbg(&format!(
                "  -> st_out={} cx={} cy={} wrap={} alt={}\n",
                self.st,
                self.cx,
                self.cy,
                self.wrap,
                if self.alt.is_empty() { 0 } else { 1 }
            ));
        }
    }

    /// Port of `mux_screen_repaint()`: push dirty rows to the real terminal
    /// (batched through `tty_buf_write`, flushed per pump by mux.c).
    pub fn repaint(&mut self, x: i32, y: i32, w: i32, h: i32) {
        if w < 1 || h < 1 {
            return;
        }
        let rows = if self.h < h { self.h } else { h }; // never paint past rect
        for r in 0..rows {
            if self.all_dirty == 0 && self.view == 0 && self.dirty.get(r as usize) != Some(&1) {
                continue;
            }
            let abs_r = y + 1 + r;
            let abs_c = x + 1;
            if self.view == 0 {
                let base = (r * self.w) as usize;
                let bytes = self.repaint_row_bytes(
                    &self.cells[base..base + self.w as usize],
                    self.w,
                    abs_r,
                    abs_c,
                );
                util::tty_buf_write(&bytes);
            } else {
                let total = self.sb_count + self.h;
                let mut top_v = total - self.h - self.view;
                if top_v < 0 {
                    top_v = 0;
                }
                let v = top_v + r;
                if v < self.sb_count {
                    if let Some((row, roww)) = self.sb_row(v) {
                        let bytes = self.repaint_row_bytes(row, roww, abs_r, abs_c);
                        util::tty_buf_write(&bytes);
                    } // !row → C `continue`
                } else {
                    let lr = v - self.sb_count;
                    if lr >= self.h {
                        continue;
                    }
                    let base = (lr * self.w) as usize;
                    let bytes = self.repaint_row_bytes(
                        &self.cells[base..base + self.w as usize],
                        self.w,
                        abs_r,
                        abs_c,
                    );
                    util::tty_buf_write(&bytes);
                }
            }
        }

        if self.view == 0 {
            for d in self.dirty.iter_mut() {
                *d = 0;
            }
            self.all_dirty = 0;
        }
    }

    /// Port of `mux_screen_free()` (mux.c `pane_free`): drop everything and
    /// zero the struct — `cells` becomes empty, so the next init()
    /// re-initializes (C: `!p->scr.cells` checks the same thing).
    /// `reply` is cleared too, exactly like C's trailing memset.
    pub fn free(&mut self) {
        *self = MuxScreen {
            w: 0,
            h: 0,
            cells: Vec::new(),
            alt: Vec::new(),
            cx: 0,
            cy: 0,
            wrap: 0,
            autowrap: 0,
            attrs: 0,
            fg: MuxColor::default(),
            bg: MuxColor::default(),
            top: 0,
            bot: 0,
            scx: 0,
            scy: 0,
            sattrs: 0,
            sfg: MuxColor::default(),
            sbg: MuxColor::default(),
            ssaved: 0,
            asx: 0,
            asy: 0,
            cur_vis: 0,
            mouse: 0,
            sb: Vec::new(),
            sb_ws: Vec::new(),
            sb_head: 0,
            sb_count: 0,
            view: 0,
            dirty: Vec::new(),
            all_dirty: 0,
            st: 0,
            csib: [0; 40],
            csin: 0,
            cp_p: [0; 16],
            cp_n: 0,
            csi_priv: 0,
            u_buf: [0; 4],
            u_have: 0,
            u_need: 0,
            last_cp: 0,
            reply: None,
        };
    }

    // ── helpers (ports of screen.c's static functions) ────────────────

    fn mark_dirty(&mut self, r: i32) {
        if r >= 0 && r < self.h {
            if let Some(d) = self.dirty.get_mut(r as usize) {
                *d = 1;
            }
        }
    }

    fn mark_all(&mut self) {
        self.all_dirty = 1;
        for d in self.dirty.iter_mut() {
            *d = 1;
        }
    }

    /// Port of `sb_push()`: ring of MUX_SCROLLBACK rows; an empty inner
    /// Vec is C's NULL slot.
    fn sb_push(&mut self, row: &[MuxCell]) {
        if self.sb.is_empty() {
            self.sb = vec![Vec::new(); MUX_SCROLLBACK];
            self.sb_ws = vec![0; MUX_SCROLLBACK];
        }
        if self.sb_count == MUX_SCROLLBACK as i32 {
            self.sb[self.sb_head as usize] = Vec::new(); // evict the oldest row
        } else {
            self.sb_count += 1;
        }
        let copy: Vec<MuxCell> = row.iter().take(self.w as usize).copied().collect();
        self.sb[self.sb_head as usize] = copy;
        self.sb_ws[self.sb_head as usize] = self.w;
        self.sb_head = (self.sb_head + 1) % MUX_SCROLLBACK as i32;
    }

    /// Port of `sb_row()`: oldest ring index = (head - count + idx + CAP) % CAP.
    /// Returns the row and its width (C's out-param).
    fn sb_row(&self, idx: i32) -> Option<(&[MuxCell], i32)> {
        if idx < 0 || idx >= self.sb_count {
            return None;
        }
        let cap = MUX_SCROLLBACK as i32;
        let i = (self.sb_head - self.sb_count + idx + cap) % cap;
        let slot = self.sb.get(i as usize)?;
        if slot.is_empty() {
            return None; // C: !s->sb[i]
        }
        Some((slot.as_slice(), self.sb_ws[i as usize]))
    }

    fn region_shift_up(&mut self, n: i32) {
        let w = self.w as usize;
        for _ in 0..n {
            if self.top == 0 && self.bot == self.h - 1 {
                let start = (self.top * self.w) as usize;
                let row: Vec<MuxCell> = self.cells[start..start + w].to_vec();
                self.sb_push(&row);
            }
            let top = self.top as usize;
            let bot = self.bot as usize;
            let len = ((self.bot - self.top) * self.w) as usize;
            self.cells
                .copy_within((top + 1) * w..(top + 1) * w + len, top * w);
            for c in 0..w {
                self.cells[bot * w + c] = MuxCell::default();
            }
        }
        for r in self.top..=self.bot {
            self.mark_dirty(r);
        }
    }

    fn region_shift_down(&mut self, n: i32) {
        let w = self.w as usize;
        for _ in 0..n {
            let top = self.top as usize;
            let len = ((self.bot - self.top) * self.w) as usize;
            self.cells.copy_within(top * w..top * w + len, (top + 1) * w);
            for c in 0..w {
                self.cells[top * w + c] = MuxCell::default();
            }
        }
        for r in self.top..=self.bot {
            self.mark_dirty(r);
        }
    }

    fn do_lf(&mut self) {
        if self.cy >= self.bot {
            self.region_shift_up(1);
        } else if self.cy < self.h - 1 {
            self.cy += 1;
        }
    }

    fn clear_cells(&mut self, mut r0: i32, mut c0: i32, mut r1: i32, mut c1: i32) {
        if r0 < 0 {
            r0 = 0;
        }
        if c0 < 0 {
            c0 = 0;
        }
        if r1 > self.h - 1 {
            r1 = self.h - 1;
        }
        if c1 > self.w - 1 {
            c1 = self.w - 1;
        }
        let bg = self.bg; // erase keeps the current background only (bce-ish)
        let w = self.w;
        let mut r = r0;
        while r <= r1 {
            /* never split a wide pair at the rectangle's edges: take the
             * glyph whose pad would be erased, and the pad whose glyph
             * would be erased */
            let base = (r * w) as usize;
            let (mut a, mut b) = (c0, c1);
            // DEVIATION: C probes row[a]/row[b] unconditionally, before any
            // a<=b check — with c0 > w-1 (a DECSC/SC restore saved before a
            // shrink can leave cx past the right edge) that reads past the
            // row in C (UB). a>=0 and b<=w-1 hold either way, so skipping
            // the probes when the range is empty matches C's own (empty)
            // clearing loop without the out-of-bounds read.
            if a <= b {
                if a > 0 && self.cells[base + a as usize].pad != 0 {
                    a -= 1;
                }
                if b < w - 1
                    && self.cells[base + b as usize].cp != 0
                    && utf8::cp_width(self.cells[base + b as usize].cp) == 2
                {
                    b += 1;
                }
                let mut c = a;
                while c <= b {
                    self.cells[base + c as usize] = MuxCell {
                        cp: 0,
                        attrs: 0,
                        pad: 0,
                        fg: MuxColor::default(),
                        bg,
                    };
                    c += 1;
                }
            }
            self.mark_dirty(r);
            r += 1;
        }
    }

    fn put_cp(&mut self, cp: u32) {
        let cw = utf8::cp_width(cp);
        if cw <= 0 {
            return; // zero-width combining marks are dropped
        }

        if self.wrap != 0 {
            self.wrap = 0;
            if self.autowrap != 0 {
                self.cx = 0;
                self.do_lf();
            }
        }
        if cw == 2 && self.cx == self.w - 1 {
            // a wide glyph cannot straddle the pane edge: wrap eagerly
            if self.autowrap != 0 {
                self.cx = 0;
                self.do_lf();
            } else {
                return; // no room: drop it
            }
        }
        if self.cx >= self.w {
            self.cx = self.w - 1;
        }
        if self.cy >= self.h {
            self.cy = self.h - 1;
        }
        let idx = (self.cy * self.w + self.cx) as usize;
        self.cells[idx].cp = cp;
        self.cells[idx].attrs = self.attrs;
        self.cells[idx].fg = self.fg;
        self.cells[idx].bg = self.bg;
        self.cells[idx].pad = 0;
        if cw == 2 && self.cx + 1 < self.w {
            self.cells[idx + 1] = MuxCell::default();
            self.cells[idx + 1].pad = 1;
        }
        self.mark_dirty(self.cy);
        self.last_cp = cp;
        self.cx += cw;
        if self.cx >= self.w {
            self.cx = self.w - 1;
            if self.autowrap != 0 {
                self.wrap = 1;
            }
        }
    }

    fn to_alt(&mut self, save_cursor: bool) {
        if save_cursor {
            self.asx = self.cx;
            self.asy = self.cy;
        }
        if self.alt.is_empty() {
            self.alt = std::mem::take(&mut self.cells);
            self.cells = vec![MuxCell::default(); (self.w * self.h) as usize];
            self.mark_all();
        }
        self.cx = 0;
        self.cy = 0;
        self.wrap = 0;
    }

    fn from_alt(&mut self, restore_cursor: bool) {
        if self.alt.is_empty() {
            return;
        }
        self.cells = std::mem::take(&mut self.alt); // free(cells); cells = alt; alt = NULL
        if restore_cursor {
            self.cx = if self.asx < self.w { self.asx } else { self.w - 1 };
            self.cy = if self.asy < self.h { self.asy } else { self.h - 1 };
        }
        self.wrap = 0;
        self.mark_all();
    }

    fn sgr(&mut self, p: &[i32]) {
        let n = p.len();
        if n == 0 {
            self.attrs = 0;
            self.fg = MuxColor::default();
            self.bg = MuxColor::default();
            return;
        }
        let mut i = 0;
        while i < n {
            let v = p[i];
            match v {
                0 => {
                    self.attrs = 0;
                    self.fg = MuxColor::default();
                    self.bg = MuxColor::default();
                }
                1 => self.attrs |= CA_BOLD,
                2 => self.attrs |= CA_DIM,
                3 => self.attrs |= CA_ITALIC,
                4 => self.attrs |= CA_UL,
                5 => self.attrs |= CA_BLINK,
                7 => self.attrs |= CA_REV,
                8 => self.attrs |= CA_INVIS,
                9 => self.attrs |= CA_STRIKE,
                21 | 22 => self.attrs &= !(CA_BOLD | CA_DIM),
                23 => self.attrs &= !CA_ITALIC,
                24 => self.attrs &= !CA_UL,
                25 => self.attrs &= !CA_BLINK,
                27 => self.attrs &= !CA_REV,
                28 => self.attrs &= !CA_INVIS,
                29 => self.attrs &= !CA_STRIKE,
                38 | 48 => {
                    let is_fg = v == 38;
                    let mut c = MuxColor::default();
                    if i + 1 < n && p[i + 1] == 5 && i + 2 < n {
                        c.kind = 2;
                        c.idx = p[i + 2] as u8;
                        i += 2;
                    } else if i + 1 < n && p[i + 1] == 2 && i + 4 < n {
                        c.kind = 3;
                        c.r = p[i + 2] as u8;
                        c.g = p[i + 3] as u8;
                        c.b = p[i + 4] as u8;
                        i += 4;
                    } else {
                        // C: `i = n; break;` — under `break` the assignment
                        // only exists to stop the for-loop, so just stop.
                        break;
                    }
                    if is_fg {
                        self.fg = c;
                    } else {
                        self.bg = c;
                    }
                }
                39 => self.fg = MuxColor::default(),
                49 => self.bg = MuxColor::default(),
                _ => {
                    if (30..=37).contains(&v) {
                        self.fg = MuxColor { kind: 1, idx: v as u8, r: 0, g: 0, b: 0 };
                    } else if (40..=47).contains(&v) {
                        self.bg = MuxColor { kind: 1, idx: v as u8, r: 0, g: 0, b: 0 };
                    } else if (90..=97).contains(&v) {
                        self.fg = MuxColor { kind: 1, idx: v as u8, r: 0, g: 0, b: 0 };
                    } else if (100..=107).contains(&v) {
                        self.bg = MuxColor { kind: 1, idx: v as u8, r: 0, g: 0, b: 0 };
                    }
                }
            }
            i += 1;
        }
    }

    /// Port of `csi_mode_common()` — mouse modes the child asked for.
    fn csi_mode_common(&mut self, set: bool, p: i32) {
        match p {
            1000 | 1001 => {
                self.mouse = if set { (self.mouse & !7) | 1 } else { self.mouse & !7 }
            }
            1002 => self.mouse = if set { (self.mouse & !7) | 2 } else { self.mouse & !7 },
            1003 => self.mouse = if set { (self.mouse & !7) | 4 } else { self.mouse & !7 },
            1006 => {
                if set {
                    self.mouse |= 8;
                } else {
                    self.mouse &= !8;
                }
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self) {
        let p = self.cp_p;
        let n = self.cp_n;
        let mut v = if n > 0 && p[0] > 0 { p[0] } else { 1 };
        let fin = if self.csin > 0 {
            self.csib[(self.csin - 1) as usize]
        } else {
            0
        };
        match fin {
            b'A' => {
                // CUU
                let lim = if self.cy >= self.top && self.cy <= self.bot {
                    self.top
                } else {
                    0
                };
                self.cy -= v;
                if self.cy < lim {
                    self.cy = lim;
                }
                self.wrap = 0;
            }
            b'B' => {
                // CUD
                let lim = if self.cy >= self.top && self.cy <= self.bot {
                    self.bot
                } else {
                    self.h - 1
                };
                self.cy += v;
                if self.cy > lim {
                    self.cy = lim;
                }
                self.wrap = 0;
            }
            b'C' => {
                self.cx += v;
                if self.cx > self.w - 1 {
                    self.cx = self.w - 1;
                }
                self.wrap = 0;
            }
            b'D' => {
                self.cx -= v;
                if self.cx < 0 {
                    self.cx = 0;
                }
                self.wrap = 0;
            }
            b'E' => {
                self.cx = 0;
                self.cy += v;
                if self.cy > self.h - 1 {
                    self.cy = self.h - 1;
                }
                self.wrap = 0;
            }
            b'F' => {
                self.cx = 0;
                self.cy -= v;
                if self.cy < 0 {
                    self.cy = 0;
                }
                self.wrap = 0;
            }
            b'G' => {
                self.cx = v - 1;
                if self.cx > self.w - 1 {
                    self.cx = self.w - 1;
                }
                if self.cx < 0 {
                    self.cx = 0;
                }
                self.wrap = 0;
            }
            b'd' => {
                self.cy = v - 1;
                if self.cy > self.h - 1 {
                    self.cy = self.h - 1;
                }
                if self.cy < 0 {
                    self.cy = 0;
                }
                self.wrap = 0;
            }
            b'H' | b'f' => {
                let params = &p[..n as usize];
                self.cy = cparam(params, 0, 1) - 1;
                self.cx = cparam(params, 1, 1) - 1;
                if self.cy > self.h - 1 {
                    self.cy = self.h - 1;
                }
                if self.cy < 0 {
                    self.cy = 0;
                }
                if self.cx > self.w - 1 {
                    self.cx = self.w - 1;
                }
                if self.cx < 0 {
                    self.cx = 0;
                }
                self.wrap = 0;
            }
            b'S' => self.region_shift_up(v),
            b'T' => self.region_shift_down(v),
            b'L' => {
                // IL: insert lines
                let lim = self.bot;
                if self.cy <= lim {
                    let w = self.w as usize;
                    let cy = self.cy as usize;
                    let limu = lim as usize;
                    for _ in 0..v {
                        let len = (limu - cy) * w;
                        self.cells
                            .copy_within(cy * w..cy * w + len, (cy + 1) * w);
                        for c in 0..w {
                            self.cells[cy * w + c] = MuxCell::default();
                        }
                    }
                    for r in self.cy..=lim {
                        self.mark_dirty(r);
                    }
                }
            }
            b'M' => {
                // DL: delete lines
                let lim = self.bot;
                if self.cy <= lim {
                    let w = self.w as usize;
                    let cy = self.cy as usize;
                    let limu = lim as usize;
                    for _ in 0..v {
                        let len = (limu - cy) * w;
                        self.cells
                            .copy_within((cy + 1) * w..(cy + 1) * w + len, cy * w);
                        for c in 0..w {
                            self.cells[limu * w + c] = MuxCell::default();
                        }
                    }
                    for r in self.cy..=lim {
                        self.mark_dirty(r);
                    }
                }
            }
            b'@' => {
                // ICH: insert blanks
                // DEVIATION: cy out of the grid (only reachable via a cursor
                // restore saved before a shrink) would make C compute a row
                // pointer past the grid and write through it (UB); skip.
                if self.cy >= 0 && self.cy < self.h {
                    let bg = self.bg;
                    if v > self.w - self.cx {
                        v = self.w - self.cx;
                    }
                    let base = (self.cy * self.w) as usize;
                    let w = self.w;
                    for i in (self.cx + v..w).rev() {
                        let cell = self.cells[base + (i - v) as usize];
                        self.cells[base + i as usize] = cell;
                    }
                    for i in 0..v {
                        self.cells[base + (self.cx + i) as usize] = MuxCell {
                            cp: 0,
                            attrs: 0,
                            pad: 0,
                            fg: MuxColor::default(),
                            bg,
                        };
                    }
                    fix_row_pairs(&mut self.cells[base..base + w as usize], w); // shifting may split
                    self.mark_dirty(self.cy);
                }
            }
            b'P' => {
                // DCH: delete chars
                // DEVIATION: same out-of-grid cy guard as ICH above.
                if self.cy >= 0 && self.cy < self.h {
                    if v > self.w - self.cx {
                        v = self.w - self.cx;
                    }
                    let base = (self.cy * self.w) as usize;
                    let w = self.w;
                    let mut i = self.cx;
                    while i < w - v {
                        let cell = self.cells[base + (i + v) as usize];
                        self.cells[base + i as usize] = cell;
                        i += 1;
                    }
                    let bg = self.bg;
                    let mut i = w - v;
                    while i < w {
                        self.cells[base + i as usize] = MuxCell {
                            cp: 0,
                            attrs: 0,
                            pad: 0,
                            fg: MuxColor::default(),
                            bg,
                        };
                        i += 1;
                    }
                    fix_row_pairs(&mut self.cells[base..base + w as usize], w); // shifting may split
                    self.mark_dirty(self.cy);
                }
            }
            b'X' => {
                let (cy, cx) = (self.cy, self.cx);
                self.clear_cells(cy, cx, cy, cx + v - 1);
            }
            b'J' => {
                let (cy, cx, w, h) = (self.cy, self.cx, self.w, self.h);
                match if n > 0 { p[0] } else { 0 } {
                    0 => {
                        self.clear_cells(cy, cx, cy, w - 1);
                        self.clear_cells(cy + 1, 0, h - 1, w - 1);
                    }
                    1 => {
                        self.clear_cells(cy, 0, cy, cx);
                        self.clear_cells(0, 0, cy - 1, w - 1);
                    }
                    _ => self.clear_cells(0, 0, h - 1, w - 1),
                }
            }
            b'K' => {
                let (cy, cx, w) = (self.cy, self.cx, self.w);
                match if n > 0 { p[0] } else { 0 } {
                    0 => self.clear_cells(cy, cx, cy, w - 1),
                    1 => self.clear_cells(cy, 0, cy, cx),
                    _ => self.clear_cells(cy, 0, cy, w - 1),
                }
            }
            b'r' => {
                // DECSTBM
                let params = &p[..n as usize];
                let mut t = cparam(params, 0, 1) - 1;
                let mut btm = cparam(params, 1, self.h) - 1;
                if t < 0 {
                    t = 0;
                }
                if btm > self.h - 1 {
                    btm = self.h - 1;
                }
                if t < btm {
                    self.top = t;
                    self.bot = btm;
                }
                self.cx = 0;
                self.cy = 0;
                self.wrap = 0;
            }
            b's' => {
                if self.csin == 1 || n == 0 {
                    // save cursor (no args — a parameterised CSI s does nothing)
                    self.scx = self.cx;
                    self.scy = self.cy;
                    self.sattrs = self.attrs;
                    self.sfg = self.fg;
                    self.sbg = self.bg;
                    self.ssaved = 1;
                }
            }
            b'u' => {
                if self.ssaved != 0 {
                    self.cx = self.scx;
                    self.cy = self.scy;
                    self.attrs = self.sattrs;
                    self.fg = self.sfg;
                    self.bg = self.sbg;
                    self.wrap = 0;
                }
            }
            b'm' => {
                let params = &p[..n as usize];
                self.sgr(params);
            }
            b'n' => {
                if n > 0 && p[0] == 6 && self.reply.is_some() {
                    let rb = format!("\x1b[{};{}R", self.cy + 1, self.cx + 1);
                    if let Some(r) = self.reply.as_mut() {
                        r(rb.as_bytes());
                    }
                }
            }
            b'c' => {
                if self.reply.is_some() && self.csi_priv == 0 {
                    if let Some(r) = self.reply.as_mut() {
                        r(b"\x1b[?6c");
                    }
                }
            }
            b'h' | b'l' => {
                let set = fin == b'h';
                if self.csi_priv == b'?' {
                    for i in 0..n as usize {
                        match p[i] {
                            7 => self.autowrap = if set { 1 } else { 0 },
                            25 => self.cur_vis = if set { 1 } else { 0 },
                            47 | 1047 => {
                                if set {
                                    self.to_alt(false);
                                } else {
                                    self.from_alt(false);
                                }
                            }
                            1048 => {
                                if set {
                                    self.asx = self.cx;
                                    self.asy = self.cy;
                                } else if self.asx < self.w && self.asy < self.h {
                                    self.cx = self.asx;
                                    self.cy = self.asy;
                                    self.wrap = 0;
                                }
                            }
                            1049 => {
                                if set {
                                    self.to_alt(true);
                                } else {
                                    self.from_alt(true);
                                }
                            }
                            1000 | 1001 | 1002 | 1003 | 1006 => {
                                self.csi_mode_common(set, p[i])
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {} // unsupported: ignore
        }
    }

    fn csi_reset(&mut self) {
        self.csin = 0;
        self.cp_n = 0;
        self.csi_priv = 0;
        for i in 0..16 {
            self.cp_p[i] = -1;
        }
    }

    fn csi_feed_char(&mut self, c: u8) {
        if (0x40..=0x7e).contains(&c) {
            // final byte — C drops it when csib is full (csin == 39) and
            // then dispatches on the byte already in the buffer; kept as-is
            if self.csin < 39 {
                self.csib[self.csin as usize] = c;
                self.csin += 1;
            }
            // parse params from csib[0..csin-2] (everything before final)
            let mut i = 0;
            if self.csin > 1
                && matches!(self.csib[0], b'?' | b'>' | b'<' | b'=')
            {
                self.csi_priv = self.csib[0];
                i = 1;
            }
            let csin = self.csin;
            let mut cur: i32 = -1;
            let mut sep = false;
            while i < csin - 1 {
                let ch = self.csib[i as usize];
                if ch.is_ascii_digit() {
                    if cur < 0 {
                        cur = 0;
                    }
                    cur = cur * 10 + (ch - b'0') as i32;
                    if cur > 65535 {
                        cur = 65535;
                    }
                } else if ch == b';' || ch == b':' {
                    if self.cp_n < 16 {
                        self.cp_p[self.cp_n as usize] = cur;
                        self.cp_n += 1;
                    }
                    cur = -1;
                    sep = true;
                }
                // intermediates (0x20-0x2f): ignored
                i += 1;
            }
            // a parameterless CSI (ESC[m, ESC[K, ESC[H …) must yield cp_n == 0
            // so defaults apply — appending a phantom -1 made ESC[K erase the
            // WHOLE line instead of to-end-of-line, wiping every TUI frame
            if cur >= 0 || sep || self.cp_n > 0 {
                if self.cp_n < 16 {
                    self.cp_p[self.cp_n as usize] = cur;
                    self.cp_n += 1;
                }
            }
            self.csi_dispatch();
            self.st = PT_NORM;
            return;
        }
        if c == 0x1b {
            self.csi_reset();
            self.st = PT_ESC;
            return;
        }
        if self.csin < 39 {
            self.csib[self.csin as usize] = c;
            self.csin += 1;
        }
    }

    fn feed_byte(&mut self, c: u8) {
        match self.st {
            PT_NORM => {
                if c == 0x1b {
                    self.st = PT_ESC;
                } else if c == b'\r' {
                    self.cx = 0;
                    self.wrap = 0;
                } else if c == b'\n' || c == 0x0b {
                    self.do_lf();
                    self.wrap = 0;
                } else if c == 0x08 {
                    if self.cx > 0 {
                        self.cx -= 1;
                    }
                    self.wrap = 0;
                } else if c == b'\t' {
                    self.cx = (self.cx / 8 + 1) * 8;
                    if self.cx > self.w - 1 {
                        self.cx = self.w - 1;
                    }
                    self.wrap = 0;
                } else if c == 0x07 || c == 0x0e || c == 0x0f {
                    // BEL / SO / SI: ignore
                } else if c >= 0x20 && c != 0x7f {
                    self.put_cp(c as u32);
                }
            }

            PT_ESC => {
                self.st = PT_NORM;
                match c {
                    b'[' => {
                        self.csi_reset();
                        self.st = PT_CSI;
                    }
                    b']' => {
                        self.st = PT_OSC;
                        util::muxdbg("  [st OSC]\n");
                    }
                    b'P' | b'^' | b'_' | b'X' => {
                        self.st = PT_SKIP;
                        util::muxdbg("  [st SKIP]\n");
                    }
                    b'7' => {
                        self.scx = self.cx;
                        self.scy = self.cy;
                        self.sattrs = self.attrs;
                        self.sfg = self.fg;
                        self.sbg = self.bg;
                        self.ssaved = 1;
                    }
                    b'8' => {
                        if self.ssaved != 0 {
                            self.cx = self.scx;
                            self.cy = self.scy;
                            self.attrs = self.sattrs;
                            self.fg = self.sfg;
                            self.bg = self.sbg;
                            self.wrap = 0;
                        }
                    }
                    b'M' => {
                        // RI
                        if self.cy <= self.top {
                            self.region_shift_down(1);
                        } else if self.cy > 0 {
                            self.cy -= 1;
                        }
                        self.wrap = 0;
                    }
                    b'D' => {
                        // IND
                        self.do_lf();
                        self.wrap = 0;
                    }
                    b'E' => {
                        // NEL
                        self.cx = 0;
                        self.do_lf();
                        self.wrap = 0;
                    }
                    b'(' | b')' | b'*' | b'+' => self.st = PT_CS,
                    b'=' | b'>' => {} // keypad modes: ignore
                    b'c' => {
                        // RIS: reset the grid, keep scrollback
                        self.alt = Vec::new();
                        self.cells = vec![MuxCell::default(); (self.w * self.h) as usize];
                        self.cx = 0;
                        self.cy = 0;
                        self.wrap = 0;
                        self.autowrap = 1;
                        self.attrs = 0;
                        self.fg = MuxColor::default();
                        self.bg = MuxColor::default();
                        self.top = 0;
                        self.bot = self.h - 1;
                        self.cur_vis = 1;
                        self.ssaved = 0;
                        self.mouse = 0; // RIS drops mouse modes
                        self.mark_all();
                    }
                    _ => {}
                }
            }

            PT_CSI => self.csi_feed_char(c),

            PT_OSC => {
                if c == 0x07 {
                    self.st = PT_NORM;
                    util::muxdbg("  [st NORM via BEL]\n");
                } else if c == 0x1b {
                    self.st = PT_SKIP; // expect ST backslash
                }
            }

            PT_SKIP => {
                if c == b'\\' {
                    self.st = PT_NORM;
                    util::muxdbg("  [st NORM via ST]\n");
                } else if c == 0x1b {
                    self.st = PT_ESC; // or a fresh escape
                }
                // C also has `c == 0x07 && 0` here: dead code, never taken
            }

            PT_CS => self.st = PT_NORM, // charset designator: consume one byte

            _ => {}
        }
    }

    /// Bytes one display row emits (factored out of `repaint_row` so the
    /// exact output can be unit-tested; `repaint` writes the result through
    /// the batched `tty_buf_write` exactly like C's `tw()`/`tty_buf_write`).
    /// `abs_r`/`abs_c` are 1-based terminal coords.
    fn repaint_row_bytes(&self, row: &[MuxCell], roww: i32, abs_r: i32, abs_c: i32) -> Vec<u8> {
        let w = self.w;
        let mut last = -1;
        let n = if roww < w { roww } else { w };
        for c in 0..n {
            if !cell_is_blank(&row[c as usize]) {
                last = c;
            }
        }

        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(format!("\x1b[{};{}H", abs_r, abs_c).as_bytes());
        if last < 0 {
            // empty line
            out.extend_from_slice(format!("\x1b[0m\x1b[{}X", w).as_bytes());
            return out;
        }
        let mut cur_attrs: u32 = 0xffff_ffff;
        let mut cf = MuxColor::default();
        let mut cb = MuxColor::default();
        let mut out_len = 0;
        for c in 0..=last {
            let cell = &row[c as usize];
            if cell.pad != 0 {
                continue; // painted with its glyph
            }
            if (cell.attrs as u32) != cur_attrs
                || cell.fg != cf
                || cell.bg != cb
            {
                out.extend_from_slice(emit_sgr(cell.attrs, cell.fg, cell.bg).as_bytes());
                cur_attrs = cell.attrs as u32;
                cf = cell.fg;
                cb = cell.bg;
            }
            if cell.cp != 0 {
                let mut tmp = [0u8; 8];
                let l = utf8::encode(cell.cp, &mut tmp);
                // MUST use the batched writer (see screen.c): a direct write
                // here would overtake the buffered CUP/SGR
                out.extend_from_slice(&tmp[..l]);
                out_len += utf8::cp_width(cell.cp);
            } else {
                out.push(b' ');
                out_len += 1;
            }
        }
        out.extend_from_slice(b"\x1b[0m");
        // Erase the tail INSIDE the pane only — ESC[K would run past the
        // pane's right edge and wipe the separator column / neighbour.
        if out_len < w {
            out.extend_from_slice(format!("\x1b[{}X", w - out_len).as_bytes());
        }
        out
    }
}

impl Drop for MuxScreen {
    fn drop(&mut self) {
        // Port of `mux_screen_free()` — vectors free themselves; keep
        // the hook for parity of any explicit cleanup added later.
    }
}

// ── free functions (screen.c statics) ───────────────────────────────────

/// Port of `grid_shrink_copy()`: copy the overlapping top-left rectangle
/// of `old` (ow×oh) into a fresh nw×nh grid of blanks.
fn shrink_copy(old: &[MuxCell], ow: i32, oh: i32, nw: i32, nh: i32) -> Vec<MuxCell> {
    let mut g = vec![MuxCell::default(); (nw * nh) as usize];
    let cr = if ow < nw { ow } else { nw };
    let chr = if oh < nh { oh } else { nh };
    for r in 0..chr {
        let src = (r * ow) as usize;
        let dst = (r * nw) as usize;
        for c in 0..cr {
            if let Some(&cell) = old.get(src + c as usize) {
                g[dst + c as usize] = cell;
            }
        }
    }
    g
}

/// Drop half of any split wide pair on a row: a 2-column glyph without
/// its padding cell (or a padding cell without its glyph) would paint a
/// stray column. Called after cell-shifting edits and grid resizes.
/// `row` may be longer than `w`; only the first `w` columns are scanned.
fn fix_row_pairs(row: &mut [MuxCell], w: i32) {
    let mut c = 0;
    while c < w {
        let cw = if row[c as usize].cp != 0 {
            utf8::cp_width(row[c as usize].cp)
        } else {
            0
        };
        if cw == 2 {
            if c + 1 >= w || row[(c + 1) as usize].pad == 0 {
                row[c as usize] = MuxCell::default(); // orphan glyph
            } else {
                c += 1; // skip its pad
            }
        } else if row[c as usize].pad != 0
            && !(c > 0
                && row[(c - 1) as usize].cp != 0
                && utf8::cp_width(row[(c - 1) as usize].cp) == 2)
        {
            row[c as usize] = MuxCell::default(); // orphan pad
        }
        c += 1;
    }
}

/// Port of `cell_is_blank()` — note C checks only the color *kinds*.
fn cell_is_blank(c: &MuxCell) -> bool {
    c.cp == 0 && c.attrs == 0 && c.fg.kind == 0 && c.bg.kind == 0
}

/// Port of `emit_sgr()`: `ESC[` + attrs (always starting with `0`) + fg +
/// bg + `m`. The C snprintfs never truncate for the values they receive
/// (worst case ≈ 56 chars into 96), so `format!` is byte-identical here.
fn emit_sgr(attrs: u8, fg: MuxColor, bg: MuxColor) -> String {
    // C builds ";0;1;…" then prints from a+1 → starts with "0"
    let mut a = String::from("0");
    if attrs & CA_BOLD != 0 {
        a.push_str(";1");
    }
    if attrs & CA_DIM != 0 {
        a.push_str(";2");
    }
    if attrs & CA_ITALIC != 0 {
        a.push_str(";3");
    }
    if attrs & CA_UL != 0 {
        a.push_str(";4");
    }
    if attrs & CA_BLINK != 0 {
        a.push_str(";5");
    }
    if attrs & CA_REV != 0 {
        a.push_str(";7");
    }
    if attrs & CA_INVIS != 0 {
        a.push_str(";8");
    }
    if attrs & CA_STRIKE != 0 {
        a.push_str(";9");
    }
    let f = match fg.kind {
        1 => format!(";{}", fg.idx),
        2 => format!(";38;5;{}", fg.idx),
        3 => format!(";38;2;{};{};{}", fg.r, fg.g, fg.b),
        _ => ";39".to_string(),
    };
    let b = match bg.kind {
        1 => format!(";{}", bg.idx),
        2 => format!(";48;5;{}", bg.idx),
        3 => format!(";48;2;{};{};{}", bg.r, bg.g, bg.b),
        _ => ";49".to_string(),
    };
    format!("\x1b[{}{}{}m", a, f, b)
}

/// Port of `cparam()`: missing or negative parameter → default.
fn cparam(p: &[i32], i: usize, defv: i32) -> i32 {
    if i >= p.len() || p[i] < 0 {
        defv
    } else {
        p[i]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scr(w: i32, h: i32) -> MuxScreen {
        let mut s = MuxScreen::default();
        s.init(w, h);
        s
    }

    fn cell(s: &MuxScreen, r: i32, c: i32) -> MuxCell {
        s.cells[(r * s.w + c) as usize]
    }

    fn row_text(s: &MuxScreen, r: i32) -> String {
        (0..s.w)
            .map(|c| {
                let cp = cell(s, r, c).cp;
                if cp == 0 {
                    ' '
                } else {
                    char::from_u32(cp).unwrap_or('?')
                }
            })
            .collect()
    }

    fn color(kind: u8, idx: u8, r: u8, g: u8, b: u8) -> MuxColor {
        MuxColor { kind, idx, r, g, b }
    }

    const DEF: MuxColor = MuxColor { kind: 0, idx: 0, r: 0, g: 0, b: 0 };
    const WIDE: &[u8] = "中".as_bytes(); // 中 — 3-byte UTF-8, width 2

    /// Wide-glyph tests need a UTF-8 locale: the C `main()` calls
    /// `setlocale(LC_ALL, "")`, but lib tests must do it themselves or
    /// `wcwidth` runs in the C locale and reports width 1 (→ -1 → 1).
    /// Process-global state → callers hold `util::test_lock()`.
    fn ensure_utf8_locale() {
        unsafe {
            libc::setlocale(libc::LC_ALL, b"\0".as_ptr().cast());
        }
    }

    #[test]
    fn init_prints_and_tracks_cursor() {
        let mut s = scr(10, 3);
        assert_eq!((s.w, s.h), (10, 3));
        assert_eq!((s.top, s.bot), (0, 2));
        assert_eq!(s.autowrap, 1);
        assert_eq!(s.cur_vis, 1);
        assert_eq!(s.st, PT_NORM);
        s.feed(b"ab");
        assert_eq!(cell(&s, 0, 0).cp, b'a' as u32);
        assert_eq!(cell(&s, 0, 1).cp, b'b' as u32);
        assert_eq!((s.cx, s.cy, s.wrap), (2, 0, 0));
        assert_eq!(s.last_cp, b'b' as u32);
        assert_eq!(s.dirty[0], 1); // print marks the row dirty
    }

    #[test]
    fn cup_defaults_clamps_and_empty_params() {
        let mut s = scr(10, 5);
        s.feed(b"\x1b[H"); // ESC[H → 1,1
        assert_eq!((s.cx, s.cy), (0, 0));
        s.feed(b"\x1b[3;4H");
        assert_eq!((s.cx, s.cy), (3, 2));
        s.feed(b"\x1b[;5H"); // missing first param → default 1
        assert_eq!((s.cx, s.cy), (4, 0));
        s.feed(b"\x1b[99;99H"); // clamped to the grid
        assert_eq!((s.cx, s.cy), (9, 4));
        s.feed(b"\x1b[f"); // HVP also homes
        assert_eq!((s.cx, s.cy), (0, 0));
        s.feed(b"\x1b[2G"); // CHA
        assert_eq!(s.cx, 1);
        s.feed(b"\x1b[4d"); // VPA
        assert_eq!(s.cy, 3);
        assert_eq!(s.wrap, 0); // every addressing CSI clears deferred wrap
    }

    #[test]
    fn cursor_moves_and_scroll_region_clamping() {
        let mut s = scr(10, 10);
        s.feed(b"\x1b[5;5H");
        s.feed(b"\x1b[3A");
        assert_eq!(s.cy, 1); // CUU
        s.feed(b"\x1b[2B");
        assert_eq!(s.cy, 3); // CUD
        s.feed(b"\x1b[4C");
        assert_eq!(s.cx, 8); // CUF
        s.feed(b"\x1b[100D");
        assert_eq!(s.cx, 0); // CUB clamps at 0
        s.feed(b"\x1b[5G");
        assert_eq!(s.cx, 4);
        s.feed(b"\x1b[9d");
        assert_eq!(s.cy, 8);
        s.feed(b"\x1b[2E"); // CNL
        assert_eq!((s.cx, s.cy), (0, 9));
        s.feed(b"\x1b[3F"); // CPL
        assert_eq!((s.cx, s.cy), (0, 6));

        s.feed(b"\x1b[3;6r"); // DECSTBM: top=2, bot=5, cursor home
        assert_eq!((s.top, s.bot), (2, 5));
        assert_eq!((s.cx, s.cy), (0, 0));
        s.feed(b"\x1b[3A"); // cursor outside region → clamp at 0
        assert_eq!(s.cy, 0);
        s.feed(b"\x1b[20B"); // outside region → clamp at h-1
        assert_eq!(s.cy, 9);
        s.feed(b"\x1b[4;1H"); // inside the region
        s.feed(b"\x1b[10A"); // CUU clamps at top
        assert_eq!(s.cy, 2);
        s.feed(b"\x1b[10B"); // CUD clamps at bot
        assert_eq!(s.cy, 5);
    }

    #[test]
    fn sgr_pen_colors_and_attrs() {
        let mut s = scr(10, 3);
        s.feed(b"\x1b[1;4;31;44mX");
        assert_eq!(
            cell(&s, 0, 0),
            MuxCell {
                cp: b'X' as u32,
                attrs: CA_BOLD | CA_UL,
                pad: 0,
                fg: color(1, 31, 0, 0, 0),
                bg: color(1, 44, 0, 0, 0),
            }
        );
        s.feed(b"\x1b[0mY");
        assert_eq!(cell(&s, 0, 1).attrs, 0);
        assert_eq!(cell(&s, 0, 1).fg, DEF);
        assert_eq!(cell(&s, 0, 1).bg, DEF);

        s.feed(b"\x1b[38;5;9;48;2;10;20;30mZ");
        assert_eq!(cell(&s, 0, 2).fg, color(2, 9, 0, 0, 0));
        assert_eq!(cell(&s, 0, 2).bg, color(3, 0, 10, 20, 30));

        s.feed(b"\x1b[39;49mV");
        assert_eq!(cell(&s, 0, 3).fg, DEF);
        assert_eq!(cell(&s, 0, 3).bg, DEF);

        s.feed(b"\x1b[1;9mQ");
        assert_eq!(cell(&s, 0, 4).attrs, CA_BOLD | CA_STRIKE);
        s.feed(b"\x1b[21;29m"); // 21 clears bold+dim, 29 strike
        s.feed(b"R");
        assert_eq!(cell(&s, 0, 5).attrs, 0);

        s.feed(b"\x1b[2m"); // DIM
        s.feed(b"\x1b[;m"); // params are [-1,-1] → sgr ignores → NO reset
        s.feed(b"S");
        assert_eq!(cell(&s, 0, 6).attrs, CA_DIM);

        // malformed 38/48 aborts the remaining params (C: i = n; break)
        s.feed(b"\x1b[1;38;5m");
        s.feed(b"T");
        assert_eq!(cell(&s, 0, 7).attrs, CA_DIM | CA_BOLD);
        assert_eq!(cell(&s, 0, 7).fg, DEF);

        // well-formed 38/48 consume their sub-params, loop continues
        s.feed(b"\x1b[38;5;9;1m");
        s.feed(b"U");
        assert_eq!(cell(&s, 0, 8).fg, color(2, 9, 0, 0, 0));
        assert_eq!(cell(&s, 0, 8).attrs, CA_DIM | CA_BOLD);
    }

    #[test]
    fn ed_el_ech_erase_with_current_bg() {
        let mut s = scr(8, 3);
        s.feed(b"\x1b[44mabcdefgh");
        s.feed(b"\x1b[1;4H\x1b[K"); // EL 0: cursor..end of line
        for c in 0..3 {
            assert_eq!(cell(&s, 0, c).cp, (b'a' + c as u8) as u32);
        }
        for c in 3..8 {
            let e = cell(&s, 0, c);
            assert_eq!((e.cp, e.attrs, e.fg, e.bg), (0, 0, DEF, color(1, 44, 0, 0, 0)));
        }

        s.feed(b"\x1b[1;4H\x1b[1K"); // EL 1: start of line..cursor (incl.)
        for c in 0..8 {
            assert_eq!(cell(&s, 0, c).cp, 0);
        }
        s.feed(b"\x1b[2J"); // ED 2: whole grid, current bg kept
        for r in 0..3 {
            for c in 0..8 {
                let e = cell(&s, r, c);
                assert_eq!((e.cp, e.bg), (0, color(1, 44, 0, 0, 0)), "cell {r},{c}");
            }
        }
        s.feed(b"\x1b[m\x1b[2J"); // default pen → erase to default bg
        assert_eq!(cell(&s, 1, 1).bg, DEF);

        s.feed(b"\x1b[1;1Hxyz\x1b[1;2H\x1b[2X"); // ECH: cols 1..2 of row 0
        assert_eq!(row_text(&s, 0), "x       "); // "x" + 7 blanks (w=8)

        s.feed(b"\x1b[1;1Htop\x1b[2;1Habcd\x1b[2;3H\x1b[1J"); // ED 1
        assert_eq!(row_text(&s, 0), "        "); // rows above the cursor
        assert_eq!(row_text(&s, 1), "   d    "); // cols 0..cursor cleared
    }

    #[test]
    fn scroll_region_lf_scrollback_and_su_sd() {
        let mut s = scr(4, 3);
        s.feed(b"1111\r\n2222\r\n3333"); // CR returns to col 0, LF advances
        s.feed(b"\n"); // at the bottom of the full region → scroll + push
        assert_eq!(row_text(&s, 0), "2222");
        assert_eq!(row_text(&s, 1), "3333");
        assert_eq!(row_text(&s, 2), "    ");
        assert_eq!(s.sb_count, 1);
        let (sbrow, sbw) = s.sb_row(0).unwrap();
        assert_eq!(sbw, 4);
        let txt: String = sbrow
            .iter()
            .map(|c| if c.cp == 0 { ' ' } else { char::from_u32(c.cp).unwrap() })
            .collect();
        assert_eq!(txt, "1111");

        // partial region: scrolls, but never pushes to scrollback
        s.feed(b"\x1b[2;3r"); // top=1, bot=2
        s.feed(b"\n\n\n"); // cy 0→1→2, third one shifts the region
        assert_eq!(row_text(&s, 0), "2222"); // outside the region: untouched
        assert_eq!(s.sb_count, 1); // "3333" fell off without a push

        s.feed(b"\x1b[r"); // back to the full region
        s.feed(b"\x1b[1;1HAAAA\x1b[2;1HBBBB\x1b[3;1HCCCC");
        s.feed(b"\x1b[1S"); // SU: full region → pushes
        assert_eq!(row_text(&s, 0), "BBBB");
        assert_eq!(row_text(&s, 1), "CCCC");
        assert_eq!(row_text(&s, 2), "    ");
        assert_eq!(s.sb_count, 2);
        s.feed(b"\x1b[1T"); // SD
        assert_eq!(row_text(&s, 0), "    ");
        assert_eq!(row_text(&s, 1), "BBBB");
        assert_eq!(row_text(&s, 2), "CCCC");
        assert_eq!(s.sb_count, 2);
    }

    #[test]
    fn decsc_decrc_and_csi_s_u() {
        let mut s = scr(10, 5);
        s.feed(b"\x1b[31m\x1b[2;3H\x1b7"); // DECSC saves cursor + pen
        assert_eq!((s.scx, s.scy), (2, 1));
        assert_eq!(s.ssaved, 1);
        s.feed(b"\x1b[5;5H\x1b[1m\x1b[32m");
        s.feed(b"\x1b8"); // DECRC
        assert_eq!((s.cx, s.cy), (2, 1));
        assert_eq!(s.attrs, 0);
        assert_eq!(s.fg, color(1, 31, 0, 0, 0));
        assert_eq!(s.wrap, 0);

        s.feed(b"\x1b[4;4H\x1b[7m\x1b[s"); // CSI s (no params) saves
        s.feed(b"\x1b[1;1H\x1b[0m");
        s.feed(b"\x1b[u");
        assert_eq!((s.cx, s.cy), (3, 3));
        assert_eq!(s.attrs, CA_REV);

        s.feed(b"\x1b[9;9H\x1b[1s"); // parameterised CSI s does NOT save
        s.feed(b"\x1b[1;1H\x1b[u"); // …so this still restores 4,4
        assert_eq!((s.cx, s.cy), (3, 3));
    }

    #[test]
    fn alt_screen_1049_swap_and_47_without_cursor_restore() {
        let mut s = scr(6, 3);
        s.feed(b"prim");
        s.feed(b"\x1b[?1049h");
        assert!(!s.alt.is_empty());
        assert_eq!(row_text(&s, 0), "      "); // fresh grid
        assert_eq!((s.cx, s.cy, s.wrap), (0, 0, 0));
        assert_eq!((s.asx, s.asy), (4, 0)); // cursor saved by 1049
        assert_eq!(s.all_dirty, 1);
        assert_eq!(s.dirty, vec![1, 1, 1]);

        s.feed(b"alt");
        assert_eq!(row_text(&s, 0), "alt   ");
        s.feed(b"\x1b[?1049l");
        assert!(s.alt.is_empty());
        assert_eq!(row_text(&s, 0), "prim  "); // content restored
        assert_eq!((s.cx, s.cy), (4, 0)); // cursor restored
        assert_eq!(s.all_dirty, 1);

        // 47: swaps the grid but never saves/restores the cursor
        s.feed(b"\x1b[?47h");
        assert_eq!((s.cx, s.cy), (0, 0));
        s.feed(b"zz");
        s.feed(b"\x1b[?47l");
        assert_eq!(row_text(&s, 0), "prim  ");
        assert_eq!((s.cx, s.cy), (2, 0)); // NOT restored to 4,0
    }

    #[test]
    fn deferred_wrap_and_autowrap_off() {
        let mut s = scr(4, 3);
        s.feed(b"abcd"); // fills the row → deferred wrap
        assert_eq!((s.cx, s.wrap), (3, 1));
        s.feed(b"e"); // wrap fires: next line, col 0
        assert_eq!((s.cx, s.cy, s.wrap), (1, 1, 0));
        assert_eq!(row_text(&s, 0), "abcd");
        assert_eq!(row_text(&s, 1), "e   ");

        s.feed(b"\x1b[?7l\x1b[H"); // DECAWM off
        s.feed(b"1234");
        assert_eq!((s.cx, s.wrap), (3, 0)); // no deferred wrap without autowrap
        s.feed(b"e"); // overwrites the last column instead
        assert_eq!(row_text(&s, 0), "123e");
        assert_eq!((s.cx, s.wrap), (3, 0));

        s.feed(b"\x1b[?7h"); // autowrap back on (kept for later tests)
        assert_eq!(s.autowrap, 1);
    }

    #[test]
    fn wide_glyphs_pad_edge_wrap_and_resize_fix() {
        let _g = crate::util::test_lock();
        ensure_utf8_locale();
        assert_eq!(
            utf8::cp_width(0x4e2d),
            2,
            "wide-glyph test needs a UTF-8 locale (LANG/LC_ALL)"
        );

        let mut s = scr(6, 3);
        let mut data = WIDE.to_vec();
        data.push(b'x');
        s.feed(&data);
        assert_eq!(cell(&s, 0, 0).cp, 0x4e2d);
        assert_eq!(
            cell(&s, 0, 1),
            MuxCell { cp: 0, attrs: 0, pad: 1, fg: DEF, bg: DEF }
        );
        assert_eq!(cell(&s, 0, 2).cp, b'x' as u32);
        assert_eq!(s.cx, 3);

        s.feed(b"\x1b[1;5H"); // cx = 4, room for a wide pair
        s.feed(WIDE);
        assert_eq!(cell(&s, 0, 4).cp, 0x4e2d);
        assert_eq!(cell(&s, 0, 5).pad, 1);
        assert_eq!((s.cx, s.wrap), (5, 1)); // hit the right edge

        s.feed(WIDE); // deferred wrap fires first
        assert_eq!(cell(&s, 1, 0).cp, 0x4e2d);
        assert_eq!(cell(&s, 1, 1).pad, 1);
        assert_eq!((s.cx, s.cy, s.wrap), (2, 1, 0));

        s.feed(b"\x1b[2;6H"); // cy=1, cx=w-1: wide glyph wraps eagerly
        s.feed(WIDE);
        assert_eq!(cell(&s, 2, 0).cp, 0x4e2d);
        assert_eq!(cell(&s, 2, 1).pad, 1);
        assert_eq!((s.cx, s.cy), (2, 2));

        s.feed(b"\x1b[?7l\x1b[3;6H"); // autowrap off at the edge → dropped
        s.feed(WIDE);
        assert_eq!(cell(&s, 2, 5).cp, 0);
        assert_eq!((s.cx, s.cy), (5, 2));

        s.feed(b"\x1b[?7h\x1b[1;4H");
        let before = s.cx;
        s.feed(b"\xcc\x81"); // U+0301 combining acute: width 0 → dropped
        assert_eq!(s.cx, before);
        assert_eq!(cell(&s, 0, 3).cp, 0);

        // resize must not leave a glyph whose pad was cut off at the edge
        s.feed(b"\x1b[1;5H");
        s.feed(WIDE); // pair at cols 4-5
        assert_eq!(cell(&s, 0, 4).cp, 0x4e2d);
        s.resize(5, 3);
        assert_eq!(cell(&s, 0, 4).cp, 0); // orphan glyph dropped
        assert_eq!(s.w, 5);
        s.resize(6, 3); // grow again: col 5 blank
        assert_eq!(cell(&s, 0, 5).cp, 0);

        // wide repaint bytes: glyph painted once, pad skipped, tail erased
        let mut w2 = scr(5, 1);
        w2.feed(WIDE);
        let bytes = w2.repaint_row_bytes(&w2.cells[0..5], 5, 1, 1);
        assert_eq!(
            bytes,
            b"\x1b[1;1H\x1b[0;39;49m\xe4\xb8\xad\x1b[0m\x1b[3X"
        );
    }

    #[test]
    fn utf8_split_across_feeds_and_parser_pauses() {
        let mut s = scr(10, 3);
        s.feed(b"\xe4\xb8"); // lead + first continuation
        assert_eq!(s.cx, 0);
        assert_eq!((s.u_have, s.u_need), (2, 3));
        s.feed(b"\xad"); // rest of 中
        assert_eq!(cell(&s, 0, 0).cp, 0x4e2d);
        assert_eq!((s.u_have, s.u_need), (0, 0));
        assert_eq!(s.cx, 1);

        s.feed(b"\x80"); // invalid lead byte → raw byte printed
        assert_eq!(cell(&s, 0, 1).cp, 0x80);
        assert_eq!(s.cx, 2);

        // a pending continuation outranks the parser: U+4E2D needs TWO more
        // bytes, so both the ESC and the '[' of "\x1b[H" are eaten as
        // (failing) continuation bytes and only 'H' prints as plain text.
        s.feed(b"\xe4");
        assert_eq!((s.u_have, s.u_need), (1, 3));
        s.feed(b"\x1b[H");
        assert_eq!(cell(&s, 0, 2).cp, b'H' as u32);
        assert_eq!(cell(&s, 0, 3).cp, 0);
        assert_eq!((s.cx, s.st), (3, PT_NORM));

        // a CSI split across two feeds resumes mid-sequence
        let mut s2 = scr(10, 2);
        s2.feed(b"\x1b[3");
        assert_eq!(s2.st, PT_CSI);
        s2.feed(b"1mZ");
        assert_eq!(s2.st, PT_NORM);
        assert_eq!(cell(&s2, 0, 0).cp, b'Z' as u32);
        assert_eq!(cell(&s2, 0, 0).fg, color(1, 31, 0, 0, 0));
    }

    #[test]
    fn scrollback_ring_grows_then_evicts_oldest() {
        let mut s = scr(8, 1); // h=1: every newline scrolls
        for i in 0..1002 {
            let line = format!("L{i}\r\n"); // CR keeps each line at col 0
            s.feed(line.as_bytes());
        }
        assert_eq!(s.sb_count, MUX_SCROLLBACK as i32);
        assert_eq!(s.sb_head, 1002 % MUX_SCROLLBACK as i32);
        let text = |idx: i32| -> String {
            let (row, w) = s.sb_row(idx).unwrap();
            assert_eq!(w, 8);
            row.iter()
                .map(|c| if c.cp == 0 { ' ' } else { char::from_u32(c.cp).unwrap() })
                .collect()
        };
        // L0 and L1 were evicted: the oldest live row is L2
        assert_eq!(text(0), "L2      ");
        assert_eq!(text(1), "L3      ");
        assert_eq!(text(MUX_SCROLLBACK as i32 - 1), "L1001   "); // newest
        assert_eq!(row_text(&s, 0), "        "); // live row cleared
        assert!(s.sb_row(-1).is_none());
        assert!(s.sb_row(MUX_SCROLLBACK as i32).is_none());
    }

    #[test]
    fn repaint_row_bytes_exact_output() {
        let mut s = scr(5, 2);
        s.feed(b"\x1b[31mhi");
        let bytes = s.repaint_row_bytes(&s.cells[0..5], 5, 1, 1);
        assert_eq!(bytes, b"\x1b[1;1H\x1b[0;31;49mhi\x1b[0m\x1b[3X");

        // blank row → CUP + reset + full-width erase
        let bytes = s.repaint_row_bytes(&s.cells[5..10], 5, 2, 1);
        assert_eq!(bytes, b"\x1b[2;1H\x1b[0m\x1b[5X");

        // attribute change mid-row emits a fresh SGR
        let mut s = scr(5, 1);
        s.feed(b"a\x1b[1mb");
        let bytes = s.repaint_row_bytes(&s.cells[0..5], 5, 1, 1);
        assert_eq!(
            bytes,
            b"\x1b[1;1H\x1b[0;39;49ma\x1b[0;1;39;49mb\x1b[0m\x1b[3X"
        );

        // erased cells keep their bg: painted as spaces, no tail erase
        let mut s = scr(5, 1);
        s.feed(b"\x1b[41mab\x1b[K");
        let bytes = s.repaint_row_bytes(&s.cells[0..5], 5, 1, 1);
        assert_eq!(bytes, b"\x1b[1;1H\x1b[0;39;41mab   \x1b[0m");
    }

    #[test]
    fn repaint_clears_dirty_only_when_live() {
        let _g = crate::util::test_lock();
        let mut s = scr(3, 2);
        assert_eq!(s.all_dirty, 1);
        s.repaint(0, 0, 3, 2); // output lands in the batched tty buffer
        assert_eq!(s.all_dirty, 0);
        assert_eq!(s.dirty, vec![0, 0]);

        s.dirty[1] = 1;
        s.repaint(1, 1, 3, 2);
        assert_eq!(s.dirty, vec![0, 0]);

        // degenerate rect: C returns before touching anything
        s.dirty[0] = 1;
        s.repaint(0, 0, 0, 2);
        assert_eq!(s.dirty, vec![1, 0]);

        // scrolled-back view: repaints every call, never clears the flags
        s.view = 1;
        s.all_dirty = 1;
        s.repaint(0, 0, 3, 2);
        assert_eq!(s.all_dirty, 1);
        s.dirty[0] = 1;
        s.repaint(0, 0, 3, 2);
        assert_eq!(s.dirty[0], 1);
    }

    #[test]
    fn mouse_mode_tracking() {
        let mut s = scr(10, 3);
        s.feed(b"\x1b[?1000h");
        assert_eq!(s.mouse & 7, 1);
        s.feed(b"\x1b[?1002h");
        assert_eq!(s.mouse & 7, 2);
        s.feed(b"\x1b[?1003h");
        assert_eq!(s.mouse & 7, 4);
        s.feed(b"\x1b[?1006h");
        assert_eq!(s.mouse & 8, 8);
        assert_eq!(s.mouse & 7, 4);
        s.feed(b"\x1b[?1000l"); // clears the level, keeps SGR bit
        assert_eq!(s.mouse, 8);
        s.feed(b"\x1b[?1006l");
        assert_eq!(s.mouse, 0);
        s.feed(b"\x1b[?1001h");
        assert_eq!(s.mouse & 7, 1);
        // without the '?' private prefix nothing happens
        s.feed(b"\x1b[1000l");
        assert_eq!(s.mouse & 7, 1);
        // DECTCEM
        s.feed(b"\x1b[?25l");
        assert_eq!(s.cur_vis, 0);
        s.feed(b"\x1b[?25h");
        assert_eq!(s.cur_vis, 1);
    }

    #[test]
    fn reply_dsr_and_da() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let mut s = scr(10, 5);
        let got = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&got);
        s.reply = Some(Box::new(move |b: &[u8]| {
            sink.borrow_mut().extend_from_slice(b);
        }));

        s.feed(b"\x1b[6n"); // DSR 6 → cursor position (ESC [ n ; m R = 6 bytes)
        assert_eq!(got.borrow().as_slice(), b"\x1b[1;1R".as_slice());
        s.feed(b"\x1b[4;7H\x1b[6n");
        assert_eq!(got.borrow().len(), 12);
        s.feed(b"\x1b[5n"); // other DSR requests → no reply
        assert_eq!(got.borrow().len(), 12);
        s.feed(b"\x1b[c"); // primary DA (no private prefix) → 5 bytes
        assert_eq!(got.borrow().len(), 17);
        s.feed(b"\x1b[?c"); // private DA → csi_priv != 0 → silence
        s.feed(b"\x1b[>c");
        s.feed(b"\x1b[=c");
        assert_eq!(got.borrow().len(), 17);
        // quirk: the private prefix is ignored by DSR — ?6n still replies
        s.feed(b"\x1b[?6n");
        assert_eq!(got.borrow().len(), 17 + 6);

        // no reply callback installed → nothing happens, no panic
        let mut plain = scr(5, 2);
        plain.feed(b"\x1b[6n\x1b[c");
        assert!(plain.reply.is_none());
    }

    #[test]
    fn insert_delete_lines_and_chars() {
        let mut s = scr(6, 4);
        s.feed(b"aaaaaa\r\nbbbbbb\r\ncccccc\r\ndddddd");
        s.feed(b"\x1b[2;1H\x1b[2L"); // IL ×2 at row 1
        assert_eq!(row_text(&s, 0), "aaaaaa");
        assert_eq!(row_text(&s, 1), "      ");
        assert_eq!(row_text(&s, 2), "      ");
        assert_eq!(row_text(&s, 3), "bbbbbb");
        assert_eq!(s.dirty[1], 1);

        let mut s = scr(6, 4);
        s.feed(b"aaaaaa\r\nbbbbbb\r\ncccccc\r\ndddddd");
        s.feed(b"\x1b[2;1H\x1b[2M"); // DL ×2 at row 1
        assert_eq!(row_text(&s, 0), "aaaaaa");
        assert_eq!(row_text(&s, 1), "dddddd");
        assert_eq!(row_text(&s, 2), "      ");
        assert_eq!(row_text(&s, 3), "      ");

        let mut s = scr(6, 1);
        s.feed(b"abcdef");
        s.feed(b"\x1b[1;3H\x1b[2@"); // ICH at col 2
        assert_eq!(row_text(&s, 0), "ab  cd");
        s.feed(b"\x1b[1;1H\x1b[2P"); // DCH at col 0
        assert_eq!(row_text(&s, 0), "  cd  ");

        let mut s = scr(6, 1);
        s.feed(b"abcdef");
        s.feed(b"\x1b[1;5H\x1b[99@"); // ICH clamps to the remaining width
        assert_eq!(row_text(&s, 0), "abcd  ");
    }

    #[test]
    fn resize_copies_clamps_and_resets_region() {
        let mut s = scr(6, 3);
        s.feed(b"aaaaaa\r\nbbbbbb\r\ncccccc");
        assert_eq!((s.cx, s.cy), (5, 2));
        s.resize(4, 2);
        assert_eq!(row_text(&s, 0), "aaaa");
        assert_eq!(row_text(&s, 1), "bbbb");
        assert_eq!((s.cx, s.cy), (3, 1)); // clamped into the new grid
        assert_eq!((s.top, s.bot, s.wrap), (0, 1, 0)); // region resets
        assert_eq!(s.dirty, vec![1, 1]);
        assert_eq!(s.all_dirty, 1);

        // same size → early return, dirty flags untouched
        s.all_dirty = 0;
        s.dirty[0] = 0;
        s.resize(4, 2);
        assert_eq!(s.all_dirty, 0);
        assert_eq!(s.dirty, vec![0, 1]);

        // growing pads with blanks
        s.resize(6, 3);
        assert_eq!(row_text(&s, 0), "aaaa  ");
        assert_eq!(row_text(&s, 1), "bbbb  ");
        assert_eq!(row_text(&s, 2), "      ");
        assert_eq!((s.top, s.bot), (0, 2));
        assert_eq!(s.dirty, vec![1, 1, 1]);
    }

    #[test]
    fn ris_resets_grid_but_keeps_scrollback() {
        let mut s = scr(4, 2);
        s.feed(b"ab\n\n"); // second newline scrolls the full region → push
        assert_eq!(s.sb_count, 1);
        s.feed(b"\x1b[?47h"); // take the alt screen
        s.feed(b"\x1b[31m\x1b[?1000h\x1b[?25l");
        s.feed(b"\x1bc"); // RIS
        assert!(s.alt.is_empty());
        assert_eq!(row_text(&s, 0), "    ");
        assert_eq!((s.cx, s.cy, s.wrap), (0, 0, 0));
        assert_eq!(s.autowrap, 1);
        assert_eq!(s.attrs, 0);
        assert_eq!(s.fg, DEF);
        assert_eq!((s.top, s.bot), (0, 1));
        assert_eq!(s.cur_vis, 1);
        assert_eq!(s.ssaved, 0);
        assert_eq!(s.mouse, 0); // RIS drops mouse modes
        assert_eq!(s.sb_count, 1); // scrollback kept
        assert_eq!(s.all_dirty, 1);
    }

    #[test]
    fn control_chars_cr_bs_tab_lf_and_ignored_bytes() {
        let mut s = scr(20, 3);
        s.feed(b"a\tb"); // tab → next multiple of 8
        assert_eq!((s.cx, cell(&s, 0, 8).cp), (9, b'b' as u32));
        s.feed(b"\x08");
        assert_eq!(s.cx, 8);
        s.feed(b"\r");
        assert_eq!(s.cx, 0);
        s.feed(b"x"); // CR then print overwrites the first column
        assert_eq!(cell(&s, 0, 0).cp, b'x' as u32);
        s.feed(b"\x08\x08"); // BS never goes below column 0
        assert_eq!(s.cx, 0);

        s.feed(b"\x1b[1;16H\t"); // tab from col 15 → 16
        assert_eq!(s.cx, 16);
        s.feed(b"\x1b[1;18H\t"); // tab past the right edge → clamped
        assert_eq!(s.cx, 19);

        s.feed(b"\r\x1b[1;1H");
        s.feed(b"\n");
        assert_eq!(s.cy, 1);
        s.feed(b"\x0b"); // VT behaves like LF
        assert_eq!(s.cy, 2);
        let sb = s.sb_count;
        s.feed(b"\x0b"); // at the bottom → scroll (full region → push)
        assert_eq!(s.cy, 2);
        assert_eq!(s.sb_count, sb + 1);

        let before = (s.cx, s.cy, s.st, s.sb_count);
        s.feed(b"\x07\x0e\x0f\x7f\x00"); // BEL, SO, SI, DEL, NUL: ignored
        assert_eq!((s.cx, s.cy, s.st, s.sb_count), before);
    }

    #[test]
    fn osc_dcs_charset_skips_and_rep_is_ignored() {
        let mut s = scr(10, 3);
        s.feed(b"\x1b]0;title\x07ab"); // OSC terminated by BEL: payload dropped
        assert_eq!(row_text(&s, 0), "ab        ");
        assert_eq!((s.cx, s.cy), (2, 0));

        s.feed(b"\x1b[1;1H\x1b]0;more\x1b\\cd"); // OSC terminated by ST
        assert_eq!(row_text(&s, 0), "cd        ");

        s.feed(b"\x1b[2;1H\x1bP1;2|junk\x1b\\ef"); // DCS skipped to ST
        assert_eq!(row_text(&s, 1), "ef        ");

        s.feed(b"\x1b[3;1H\x1b_Bg\x1b\\h"); // APC skipped to ST
        assert_eq!(row_text(&s, 2), "h         ");

        s.feed(b"\x1b[1;1H\x1b(B"); // charset designator consumes 'B'
        assert_eq!(s.st, PT_NORM);
        s.feed(b"i");
        assert_eq!(row_text(&s, 0), "id        "); // 'i' prints, 'B' did not
        s.feed(b"\x1b=\x1b>"); // keypad modes: ignored
        assert_eq!(s.st, PT_NORM);

        // CSI b (REP) is NOT implemented by screen.c — last_cp is written
        // by put_cp but never consumed, so nothing repeats.
        let mut s2 = scr(10, 1);
        s2.feed(b"\x1b[1;1Ha\x1b[bb");
        assert_eq!(row_text(&s2, 0), "ab        ");
        assert_eq!(s2.cx, 2);

        // the private prefix is ignored by ED: ?2J behaves like 2J
        let mut s3 = scr(4, 2);
        s3.feed(b"abcd\nefgh");
        s3.feed(b"\x1b[?2J");
        assert_eq!(row_text(&s3, 0), "    ");
        assert_eq!(row_text(&s3, 1), "    ");
    }

    #[test]
    fn free_resets_like_c_memset() {
        let mut s = scr(4, 2);
        s.feed(b"ab\n\n"); // scroll once so the ring has a row
        assert_eq!(s.sb_count, 1);
        s.feed(b"\x1b[?1000h");
        let got = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = std::rc::Rc::clone(&got);
        s.reply = Some(Box::new(move |b: &[u8]| {
            sink.borrow_mut().extend_from_slice(b);
        }));
        s.free(); // C: mux_screen_free → memset(s, 0, sizeof *s)
        assert!(s.cells.is_empty()); // `!p->scr.cells` → init() on reuse
        assert!(s.alt.is_empty());
        assert_eq!((s.w, s.h), (0, 0));
        assert_eq!(s.mouse, 0);
        assert_eq!(s.sb_count, 0);
        assert_eq!(s.all_dirty, 0);
        assert!(s.reply.is_none()); // memset clears it, like C
        s.init(3, 2); // re-init works on a freed screen
        assert_eq!((s.w, s.h), (3, 2));
        assert_eq!(s.cells.len(), 6);
        assert_eq!(s.all_dirty, 1);
    }
}
