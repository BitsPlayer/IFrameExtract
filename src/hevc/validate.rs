//! Exact check of a candidate parameter set against an intra slice.
//!
//! This walks the full CABAC syntax of the slice data - SAO, coding quadtree, prediction modes,
//! transform tree, residuals - without reconstructing a single sample. The contexts it selects,
//! and therefore every bin it decodes, depend on the parameter set: block sizes, transform depth,
//! SAO, sign hiding, cu_qp_delta, transform skip, WPP, bit depth and the slice QP that seeds the
//! context tables. With any of them wrong the arithmetic decoder desynchronises, and a
//! desynchronised decoder does not finish the slice on its stop bit. With all of them right it
//! does, every time. That is a far stronger signal than anything a decoder reports: ffmpeg
//! happily renders a wrong guess as a plausible-looking picture.

use super::cabac::{Contexts, Engine, ctx};
use super::params::HevcParams;
use super::slice::{self, SliceHeader};
use crate::bits::unescape_rbsp_mapped;

/// Why a slice did not validate. Only used for diagnostics; any reason rejects the candidate.
pub type Reject = &'static str;

const INTRA_DC: u8 = 1;

/// Per-picture neighbour state at 4x4 granularity, reused across attempts. A cell belongs to the
/// slice being parsed only when its stamp matches the current generation, which keeps a fresh
/// attempt from paying for a full clear.
#[derive(Default)]
pub struct Scratch {
    w4: usize,
    h4: usize,
    stamp: Vec<u32>,
    depth: Vec<u8>,
    mode: Vec<u8>,
    generation: u32,
}

impl Scratch {
    fn prepare(&mut self, width: u32, height: u32) {
        let (w4, h4) = (width.div_ceil(4) as usize, height.div_ceil(4) as usize);
        if w4 != self.w4 || h4 != self.h4 || self.generation == u32::MAX {
            *self = Self {
                w4,
                h4,
                stamp: vec![0; w4 * h4],
                depth: vec![0; w4 * h4],
                mode: vec![0; w4 * h4],
                generation: 0,
            };
        }
        self.generation += 1;
    }
}

/// Coefficient scan orders, `[log2 block size][scanIdx]` for sizes 1, 2, 4 and 8.
struct Scans {
    orders: [[Vec<(u8, u8)>; 3]; 4],
}

impl Scans {
    fn new() -> Self {
        let make = |log2: usize| -> [Vec<(u8, u8)>; 3] {
            let n = 1usize << log2;
            let mut diag = Vec::with_capacity(n * n);
            let (mut x, mut y) = (0i32, 0i32);
            while diag.len() < n * n {
                while y >= 0 {
                    if (x as usize) < n && (y as usize) < n {
                        diag.push((x as u8, y as u8));
                    }
                    y -= 1;
                    x += 1;
                }
                y = x;
                x = 0;
            }
            let horiz = (0..n * n).map(|i| ((i % n) as u8, (i / n) as u8)).collect();
            let vert = (0..n * n).map(|i| ((i / n) as u8, (i % n) as u8)).collect();
            [diag, horiz, vert]
        };
        Self {
            orders: [make(0), make(1), make(2), make(3)],
        }
    }
}

fn scans() -> &'static Scans {
    static SCANS: std::sync::OnceLock<Scans> = std::sync::OnceLock::new();
    SCANS.get_or_init(Scans::new)
}

const CTX_IDX_MAP_4X4: [u8; 15] = [0, 1, 4, 5, 2, 3, 4, 5, 6, 6, 8, 8, 7, 7, 8];

struct Parser<'a, 's> {
    p: &'a HevcParams,
    e: Engine<'a>,
    c: Contexts,
    g: &'s mut Scratch,
    pic_w: u32,
    pic_h: u32,
    slice_addr: u32,
    sao_luma: bool,
    sao_chroma: bool,
    cu_transquant_bypass: bool,
    cu_qp_delta_coded: bool,
    log2_min_qg: u32,
    chroma_mode: u8,
    max_trafo_depth: u32,
    intra_split: bool,
}

impl Parser<'_, '_> {
    fn cell(&self, x: u32, y: u32) -> usize {
        (y as usize >> 2) * self.g.w4 + (x as usize >> 2)
    }

    fn available(&self, x: i64, y: i64) -> bool {
        x >= 0
            && y >= 0
            && (x as u32) < self.pic_w
            && (y as u32) < self.pic_h
            && self.g.stamp[self.cell(x as u32, y as u32)] == self.g.generation
    }

    fn fill(&mut self, x0: u32, y0: u32, size: u32, mut f: impl FnMut(&mut Scratch, usize)) {
        let x_end = (x0 + size).min(self.pic_w);
        let y_end = (y0 + size).min(self.pic_h);
        for y in (y0..y_end).step_by(4) {
            for x in (x0..x_end).step_by(4) {
                let i = self.cell(x, y);
                f(self.g, i);
            }
        }
    }

    fn dec(&mut self, idx: usize) -> u32 {
        self.e.decision(&mut self.c.states[idx])
    }

    fn check_overrun(&self) -> Result<(), Reject> {
        // The arithmetic decoder legitimately reads a few bits ahead; a desynchronised one runs
        // off the end of its substream and keeps going.
        if self.e.overrun > 16 {
            Err("ran past end of data")
        } else {
            Ok(())
        }
    }

    fn sao(&mut self, rx: u32, ry: u32) -> Result<(), Reject> {
        if !self.sao_luma && !self.sao_chroma {
            return Ok(());
        }
        let w = self.p.width_in_ctbs();
        let addr = ry * w + rx;
        if rx > 0 && addr > self.slice_addr && self.dec(ctx::SAO_MERGE) == 1 {
            return Ok(());
        }
        if ry > 0 && addr - w >= self.slice_addr && self.dec(ctx::SAO_MERGE) == 1 {
            return Ok(());
        }
        let c_max = (1u32 << (self.p.bit_depth.min(10) - 5)) - 1;
        let mut chroma_type = 0;
        for c_idx in 0..3 {
            if (c_idx == 0 && !self.sao_luma) || (c_idx > 0 && !self.sao_chroma) {
                continue;
            }
            let typ = if c_idx == 2 {
                chroma_type
            } else if self.dec(ctx::SAO_TYPE) == 0 {
                0
            } else if self.e.bypass() == 0 {
                1
            } else {
                2
            };
            if c_idx == 1 {
                chroma_type = typ;
            }
            if typ == 0 {
                continue;
            }
            let mut abs = [0u32; 4];
            for a in &mut abs {
                while *a < c_max && self.e.bypass() == 1 {
                    *a += 1;
                }
            }
            if typ == 1 {
                for a in abs {
                    if a != 0 {
                        self.e.bypass();
                    }
                }
                self.e.bypass_bits(5);
            } else if c_idx < 2 {
                self.e.bypass_bits(2);
            }
        }
        Ok(())
    }

    fn coding_quadtree(&mut self, x0: u32, y0: u32, log2: u32, depth: u32) -> Result<(), Reject> {
        let size = 1 << log2;
        let split =
            if x0 + size <= self.pic_w && y0 + size <= self.pic_h && log2 > self.p.log2_min_cb {
                let mut inc = 0;
                if self.available(x0 as i64 - 1, y0 as i64)
                    && self.g.depth[self.cell(x0 - 1, y0)] as u32 > depth
                {
                    inc += 1;
                }
                if self.available(x0 as i64, y0 as i64 - 1)
                    && self.g.depth[self.cell(x0, y0 - 1)] as u32 > depth
                {
                    inc += 1;
                }
                self.dec(ctx::SPLIT_CU + inc) == 1
            } else {
                log2 > self.p.log2_min_cb
            };
        if self.p.cu_qp_delta_depth.is_some() && log2 >= self.log2_min_qg {
            self.cu_qp_delta_coded = false;
        }
        if split {
            let half = size / 2;
            let (x1, y1) = (x0 + half, y0 + half);
            self.coding_quadtree(x0, y0, log2 - 1, depth + 1)?;
            if x1 < self.pic_w {
                self.coding_quadtree(x1, y0, log2 - 1, depth + 1)?;
            }
            if y1 < self.pic_h {
                self.coding_quadtree(x0, y1, log2 - 1, depth + 1)?;
            }
            if x1 < self.pic_w && y1 < self.pic_h {
                self.coding_quadtree(x1, y1, log2 - 1, depth + 1)?;
            }
            Ok(())
        } else {
            self.coding_unit(x0, y0, log2, depth)
        }
    }

    fn luma_mode(&self, x: i64, y: i64) -> u8 {
        if self.available(x, y) {
            self.g.mode[self.cell(x as u32, y as u32)]
        } else {
            INTRA_DC
        }
    }

    fn coding_unit(&mut self, x0: u32, y0: u32, log2: u32, depth: u32) -> Result<(), Reject> {
        let size = 1u32 << log2;
        self.cu_transquant_bypass =
            self.p.transquant_bypass && self.dec(ctx::TRANSQUANT_BYPASS) == 1;
        let nxn = log2 == self.p.log2_min_cb && self.dec(ctx::PART_MODE) == 0;
        if nxn && log2 <= self.p.log2_min_tb {
            return Err("NxN below minimum transform size");
        }
        self.fill(x0, y0, size, |g, i| {
            g.stamp[i] = g.generation;
            g.depth[i] = depth as u8;
        });

        let parts = if nxn { 4 } else { 1 };
        let pb = if nxn { size / 2 } else { size };
        let mut prev = [false; 4];
        for f in prev.iter_mut().take(parts) {
            *f = self.dec(ctx::PREV_INTRA_LUMA) == 1;
        }
        let mut first_mode = 0;
        for (i, &use_mpm) in prev.iter().enumerate().take(parts) {
            let xp = x0 + (i as u32 % 2) * pb;
            let yp = y0 + (i as u32 / 2) * pb;
            let a = self.luma_mode(xp as i64 - 1, yp as i64);
            // The above neighbour only counts inside the current CTB row.
            let ctb_top = (yp >> self.p.log2_ctb) << self.p.log2_ctb;
            let b = if (yp as i64 - 1) < ctb_top as i64 {
                INTRA_DC
            } else {
                self.luma_mode(xp as i64, yp as i64 - 1)
            };
            let cand = if a == b {
                if a < 2 {
                    [0, 1, 26]
                } else {
                    [a, 2 + ((a + 29) % 32), 2 + ((a - 2 + 1) % 32)]
                }
            } else {
                let c = if a != 0 && b != 0 {
                    0
                } else if a != 1 && b != 1 {
                    1
                } else {
                    26
                };
                [a, b, c]
            };
            let mode = if use_mpm {
                let idx = if self.e.bypass() == 0 {
                    0
                } else if self.e.bypass() == 0 {
                    1
                } else {
                    2
                };
                cand[idx]
            } else {
                let mut sorted = cand;
                sorted.sort_unstable();
                let mut m = self.e.bypass_bits(5) as u8;
                for s in sorted {
                    if m >= s {
                        m += 1;
                    }
                }
                m
            };
            if i == 0 {
                first_mode = mode;
            }
            self.fill(xp, yp, pb, |g, idx| g.mode[idx] = mode);
        }

        let chroma_sel = if self.dec(ctx::CHROMA_PRED) == 0 {
            4
        } else {
            self.e.bypass_bits(2)
        };
        self.chroma_mode = match chroma_sel {
            4 => first_mode,
            s => {
                let m = [0u8, 26, 10, 1][s as usize];
                if m == first_mode { 34 } else { m }
            }
        };

        self.intra_split = nxn;
        self.max_trafo_depth = self.p.max_transform_depth_intra + nxn as u32;
        self.transform_tree(x0, y0, x0, y0, log2, 0, 0, false, false)
    }

    #[allow(clippy::too_many_arguments)]
    fn transform_tree(
        &mut self,
        x0: u32,
        y0: u32,
        x_base: u32,
        y_base: u32,
        log2: u32,
        depth: u32,
        blk: u32,
        parent_cb: bool,
        parent_cr: bool,
    ) -> Result<(), Reject> {
        let split = if log2 <= self.p.log2_max_tb
            && log2 > self.p.log2_min_tb
            && depth < self.max_trafo_depth
            && !(self.intra_split && depth == 0)
        {
            self.dec(ctx::SPLIT_TRANSFORM + 5 - log2 as usize) == 1
        } else {
            log2 > self.p.log2_max_tb || (self.intra_split && depth == 0)
        };

        let (mut cb, mut cr) = (false, false);
        if log2 > 2 {
            if depth == 0 || parent_cb {
                cb = self.dec(ctx::CBF_CHROMA + depth as usize) == 1;
            }
            if depth == 0 || parent_cr {
                cr = self.dec(ctx::CBF_CHROMA + depth as usize) == 1;
            }
        } else {
            // 4:2:0 chroma for a 4x4 luma split is carried once, by the parent.
            cb = parent_cb;
            cr = parent_cr;
        }

        if split {
            let h = 1 << (log2 - 1);
            self.transform_tree(x0, y0, x0, y0, log2 - 1, depth + 1, 0, cb, cr)?;
            self.transform_tree(x0 + h, y0, x0, y0, log2 - 1, depth + 1, 1, cb, cr)?;
            self.transform_tree(x0, y0 + h, x0, y0, log2 - 1, depth + 1, 2, cb, cr)?;
            self.transform_tree(x0 + h, y0 + h, x0, y0, log2 - 1, depth + 1, 3, cb, cr)
        } else {
            let cbf_luma = self.dec(ctx::CBF_LUMA + (depth == 0) as usize) == 1;
            self.transform_unit(x0, y0, x_base, y_base, log2, blk, cbf_luma, cb, cr)
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn transform_unit(
        &mut self,
        x0: u32,
        y0: u32,
        x_base: u32,
        y_base: u32,
        log2: u32,
        blk: u32,
        cbf_luma: bool,
        cb: bool,
        cr: bool,
    ) -> Result<(), Reject> {
        if !(cbf_luma || cb || cr) {
            return Ok(());
        }
        if self.p.cu_qp_delta_depth.is_some() && !self.cu_qp_delta_coded {
            self.cu_qp_delta()?;
            self.cu_qp_delta_coded = true;
        }
        if cbf_luma {
            self.residual(x0, y0, log2, 0)?;
        }
        if log2 > 2 {
            if cb {
                self.residual(x0, y0, log2 - 1, 1)?;
            }
            if cr {
                self.residual(x0, y0, log2 - 1, 2)?;
            }
        } else if blk == 3 {
            if cb {
                self.residual(x_base, y_base, 2, 1)?;
            }
            if cr {
                self.residual(x_base, y_base, 2, 2)?;
            }
        }
        self.check_overrun()
    }

    fn cu_qp_delta(&mut self) -> Result<(), Reject> {
        let mut v = 0u32;
        while v < 5 && self.dec(ctx::CU_QP_DELTA + (v > 0) as usize) == 1 {
            v += 1;
        }
        if v == 5 {
            let mut k = 0;
            while self.e.bypass() == 1 {
                v += 1 << k;
                k += 1;
                if k > 31 {
                    return Err("cu_qp_delta escape overflow");
                }
            }
            v += self.e.bypass_bits(k);
        }
        let limit = 26 + self.p.qp_bd_offset() as u32 / 2;
        if v > 0 {
            let negative = self.e.bypass() == 1;
            if (negative && v > limit) || (!negative && v > limit - 1) {
                return Err("cu_qp_delta out of range");
            }
        }
        Ok(())
    }

    fn residual(&mut self, x0: u32, y0: u32, log2: u32, c_idx: u32) -> Result<(), Reject> {
        let chroma = c_idx > 0;
        if self.p.transform_skip && !self.cu_transquant_bypass && log2 <= 2 {
            self.dec(ctx::TRANSFORM_SKIP + chroma as usize);
        }

        let c_max = (log2 << 1) - 1;
        let (off, shift) = if chroma {
            (15, log2 - 2)
        } else {
            (3 * (log2 - 2) + ((log2 - 1) >> 2), (log2 + 1) >> 2)
        };
        let mut px = 0;
        while px < c_max && self.dec(ctx::LAST_X + (off + (px >> shift)) as usize) == 1 {
            px += 1;
        }
        let mut py = 0;
        while py < c_max && self.dec(ctx::LAST_Y + (off + (py >> shift)) as usize) == 1 {
            py += 1;
        }
        let suffix = |e: &mut Engine, prefix: u32| -> u32 {
            if prefix > 3 {
                let n = (prefix >> 1) - 1;
                (1 << n) * (2 + (prefix & 1)) + e.bypass_bits(n)
            } else {
                prefix
            }
        };
        let mut last_x = suffix(&mut self.e, px);
        let mut last_y = suffix(&mut self.e, py);

        let mode = if chroma {
            self.chroma_mode
        } else {
            self.g.mode[self.cell(x0, y0)]
        };
        let scan_idx = if log2 == 2 || (log2 == 3 && !chroma) {
            match mode {
                6..=14 => 2,
                22..=30 => 1,
                _ => 0,
            }
        } else {
            0
        };
        if scan_idx == 2 {
            std::mem::swap(&mut last_x, &mut last_y);
        }

        let sb_log2 = (log2 - 2) as usize;
        let sb_w = 1usize << sb_log2;
        let sb_scan = &scans().orders[sb_log2][scan_idx];
        let pos_scan = &scans().orders[2][scan_idx];
        let last_sb = sb_scan
            .iter()
            .position(|&(x, y)| x as u32 == last_x >> 2 && y as u32 == last_y >> 2)
            .ok_or("last position outside block")?;
        let last_pos = pos_scan
            .iter()
            .position(|&(x, y)| x as u32 == last_x & 3 && y as u32 == last_y & 3)
            .unwrap();

        let mut csbf = [[false; 8]; 8];
        let mut c1 = 1u32;
        for i in (0..=last_sb).rev() {
            let (xs, ys) = (sb_scan[i].0 as usize, sb_scan[i].1 as usize);
            let mut infer_dc = false;
            if i < last_sb && i > 0 {
                let mut inc = 0;
                if xs + 1 < sb_w && csbf[xs + 1][ys] {
                    inc = 1;
                }
                if ys + 1 < sb_w && csbf[xs][ys + 1] {
                    inc = 1;
                }
                csbf[xs][ys] = self.dec(ctx::CODED_SUB_BLOCK + inc + 2 * chroma as usize) == 1;
                infer_dc = true;
            } else {
                csbf[xs][ys] = true;
            }

            let mut sig = [false; 16];
            let start = if i == last_sb {
                sig[last_pos] = true;
                last_pos as i32 - 1
            } else {
                15
            };
            if csbf[xs][ys] {
                let prev_csbf = (xs + 1 < sb_w && csbf[xs + 1][ys]) as u32
                    + 2 * (ys + 1 < sb_w && csbf[xs][ys + 1]) as u32;
                for n in (0..=start.max(-1)).rev() {
                    let n = n as usize;
                    if n == 0 && infer_dc {
                        sig[0] = true;
                        continue;
                    }
                    let (xp, yp) = (pos_scan[n].0 as u32, pos_scan[n].1 as u32);
                    let xc = ((xs as u32) << 2) + xp;
                    let yc = ((ys as u32) << 2) + yp;
                    let sig_ctx = if log2 == 2 {
                        CTX_IDX_MAP_4X4[((yc << 2) + xc) as usize] as u32
                    } else if xc + yc == 0 {
                        0
                    } else {
                        let mut s = match prev_csbf {
                            0 => {
                                if xp + yp == 0 {
                                    2
                                } else if xp + yp < 3 {
                                    1
                                } else {
                                    0
                                }
                            }
                            1 => {
                                if yp == 0 {
                                    2
                                } else if yp == 1 {
                                    1
                                } else {
                                    0
                                }
                            }
                            2 => {
                                if xp == 0 {
                                    2
                                } else if xp == 1 {
                                    1
                                } else {
                                    0
                                }
                            }
                            _ => 2,
                        };
                        if !chroma {
                            if xs > 0 || ys > 0 {
                                s += 3;
                            }
                            if log2 == 3 {
                                s += if scan_idx == 0 { 9 } else { 15 };
                            } else {
                                s += 21;
                            }
                        } else if log2 == 3 {
                            s += 9;
                        } else {
                            s += 12;
                        }
                        s
                    };
                    let inc = if chroma { 27 + sig_ctx } else { sig_ctx };
                    sig[n] = self.dec(ctx::SIG_COEFF + inc as usize) == 1;
                    if sig[n] {
                        infer_dc = false;
                    }
                }
            }

            let positions: Vec<usize> = (0..16).rev().filter(|&n| sig[n]).collect();
            if positions.is_empty() {
                continue;
            }
            let mut ctx_set = if i == 0 || chroma { 0 } else { 2 };
            if c1 == 0 {
                ctx_set += 1;
            }
            c1 = 1;
            let mut g1 = [false; 16];
            let mut first_g1: Option<usize> = None;
            for &n in positions.iter().take(8) {
                let inc = ctx_set * 4 + c1 + if chroma { 16 } else { 0 };
                g1[n] = self.dec(ctx::GREATER1 + inc as usize) == 1;
                if g1[n] {
                    c1 = 0;
                    first_g1.get_or_insert(n);
                } else if c1 > 0 && c1 < 3 {
                    c1 += 1;
                }
            }
            let g2 = match first_g1 {
                Some(_) => {
                    self.dec(ctx::GREATER2 + ctx_set as usize + if chroma { 4 } else { 0 }) == 1
                }
                None => false,
            };
            let first_sig = *positions.last().unwrap();
            let last_sig = positions[0];
            let hidden =
                self.p.sign_data_hiding && !self.cu_transquant_bypass && last_sig - first_sig > 3;
            let signs = positions.len() - hidden as usize;
            for _ in 0..signs {
                self.e.bypass();
            }

            let mut rice = 0u32;
            for (k, &n) in positions.iter().enumerate() {
                let base = 1 + g1[n] as u32 + (first_g1 == Some(n) && g2) as u32;
                let threshold = if k < 8 {
                    if first_g1 == Some(n) { 3 } else { 2 }
                } else {
                    1
                };
                if base == threshold {
                    let rem = self.coeff_abs_level_remaining(rice)?;
                    if base + rem > 3 * (1 << rice) {
                        rice = (rice + 1).min(4);
                    }
                }
            }
        }
        Ok(())
    }

    fn coeff_abs_level_remaining(&mut self, rice: u32) -> Result<u32, Reject> {
        let mut prefix = 0u32;
        while self.e.bypass() == 1 {
            prefix += 1;
            if prefix > 32 {
                return Err("coeff_abs_level_remaining prefix overflow");
            }
        }
        Ok(if prefix <= 3 {
            (prefix << rice) + self.e.bypass_bits(rice)
        } else {
            let n = prefix - 3 + rice;
            if n > 31 {
                return Err("coeff_abs_level_remaining suffix overflow");
            }
            (((1u32 << (prefix - 3)) + 2) << rice).wrapping_add(self.e.bypass_bits(n))
        })
    }
}

/// Bit index of the last set bit in `rbsp[..end_byte]` - the stop bit or alignment bit that
/// closes a substream - or `None` if the range is all zeros.
fn last_one_bit(rbsp: &[u8], start_byte: usize, end_byte: usize) -> Option<usize> {
    let end = end_byte.min(rbsp.len());
    (start_byte..end)
        .rev()
        .find(|&i| rbsp[i] != 0)
        .map(|i| i * 8 + 7 - rbsp[i].trailing_zeros() as usize)
}

/// Outcome of validating one slice segment.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Parsed to the stop bit of every substream.
    Valid,
    /// Not something this checker covers (dependent segment); neither confirms nor rejects.
    Skipped,
}

/// Checks one slice NAL payload (two-byte header included) against `p`.
pub fn validate_slice(
    p: &HevcParams,
    payload: &[u8],
    scratch: &mut Scratch,
) -> Result<Outcome, Reject> {
    let nal_type = super::nal_type(payload);
    let (rbsp, removed) = unescape_rbsp_mapped(payload);
    let hdr: SliceHeader = slice::parse(p, nal_type, &rbsp).ok_or("slice header")?;
    if hdr.dependent {
        return Ok(Outcome::Skipped);
    }

    // Substream boundaries, converted from escaped-payload offsets to RBSP bytes.
    let data_byte = hdr.data_bit / 8;
    let escaped_start = {
        let mut e = data_byte;
        for &rem in &removed {
            if rem <= e {
                e += 1;
            } else {
                break;
            }
        }
        e
    };
    let to_rbsp = |e: usize| e - removed.iter().take_while(|&&x| x < e).count();
    let mut bounds = vec![data_byte];
    let mut esc = escaped_start;
    for &len in &hdr.entry_points {
        esc += len as usize;
        if esc >= payload.len() {
            return Err("entry point beyond payload");
        }
        bounds.push(to_rbsp(esc));
    }
    bounds.push(rbsp.len());

    scratch.prepare(p.coded_width(), p.coded_height());
    let w_ctb = p.width_in_ctbs();
    let total = w_ctb * p.height_in_ctbs();
    let mut parser = Parser {
        p,
        e: Engine::new(&rbsp, bounds[0], bounds[1]).ok_or("cabac init")?,
        c: Contexts::new(hdr.slice_qp),
        g: scratch,
        pic_w: p.coded_width(),
        pic_h: p.coded_height(),
        slice_addr: hdr.address,
        sao_luma: hdr.sao_luma,
        sao_chroma: hdr.sao_chroma,
        cu_transquant_bypass: false,
        cu_qp_delta_coded: false,
        log2_min_qg: p.log2_ctb - p.cu_qp_delta_depth.unwrap_or(0),
        chroma_mode: 0,
        max_trafo_depth: 0,
        intra_split: false,
    };

    let mut addr = hdr.address;
    let mut substream = 0usize;
    let mut wpp_saved: Option<Contexts> = None;
    loop {
        let (rx, ry) = (addr % w_ctb, addr / w_ctb);
        parser.sao(rx, ry)?;
        parser.coding_quadtree(rx << p.log2_ctb, ry << p.log2_ctb, p.log2_ctb, 0)?;
        parser.check_overrun()?;
        if p.entropy_coding_sync && rx == 1 {
            wpp_saved = Some(parser.c.clone());
        }
        let end = parser.e.terminate() == 1;
        addr += 1;
        let row_end = p.entropy_coding_sync && addr.is_multiple_of(w_ctb) && addr < total;

        if end || row_end {
            if !end && parser.e.terminate() != 1 {
                return Err("end_of_subset_one_bit is zero");
            }
            let stop = last_one_bit(&rbsp, bounds[substream], bounds[substream + 1])
                .ok_or("empty substream")?;
            // The encoder's flush makes the closing one-bit the last bit the decoder reads when it
            // decodes the terminating bin. Anywhere else means the parse drifted.
            if parser.e.bit_pos() != stop + 1 {
                return Err("substream does not end on its stop bit");
            }
            if end {
                return if substream + 2 == bounds.len() {
                    Ok(Outcome::Valid)
                } else {
                    Err("slice ended before its last entry point")
                };
            }
            substream += 1;
            if substream + 1 >= bounds.len() {
                return Err("more CTB rows than entry points");
            }
            parser.e =
                Engine::new(&rbsp, bounds[substream], bounds[substream + 1]).ok_or("cabac init")?;
            // WPP inherits contexts from the second CTB of the row above, when that CTB is in
            // this slice.
            let above_right = addr - w_ctb + 1;
            parser.c = match &wpp_saved {
                Some(saved) if w_ctb > 1 && above_right >= hdr.address => saved.clone(),
                _ => Contexts::new(hdr.slice_qp),
            };
        } else if addr >= total {
            return Err("no end_of_slice_segment_flag at the last CTB");
        }
    }
}
