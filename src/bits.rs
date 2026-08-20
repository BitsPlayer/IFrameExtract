//! Minimal bit reader/writer for H.264 RBSP, plus emulation-prevention handling.

pub struct BitWriter {
    bytes: Vec<u8>,
    bit: u8,
    cur: u8,
}

impl BitWriter {
    pub fn new() -> Self {
        Self {
            bytes: Vec::new(),
            bit: 0,
            cur: 0,
        }
    }

    pub fn u(&mut self, n: u32, v: u32) {
        for i in (0..n).rev() {
            self.cur = (self.cur << 1) | ((v >> i) & 1) as u8;
            self.bit += 1;
            if self.bit == 8 {
                self.bytes.push(self.cur);
                self.cur = 0;
                self.bit = 0;
            }
        }
    }

    pub fn flag(&mut self, v: bool) {
        self.u(1, v as u32);
    }

    pub fn ue(&mut self, v: u32) {
        let code = v + 1;
        let n = 32 - code.leading_zeros();
        self.u(n - 1, 0);
        self.u(n, code);
    }

    pub fn se(&mut self, v: i32) {
        self.ue(if v > 0 { (2 * v - 1) as u32 } else { (-2 * v) as u32 });
    }

    /// Appends the rbsp_trailing_bits and returns the raw RBSP.
    pub fn finish(mut self) -> Vec<u8> {
        self.u(1, 1);
        while self.bit != 0 {
            self.u(1, 0);
        }
        self.bytes
    }
}

/// Inserts emulation prevention bytes, turning an RBSP into a NAL payload.
pub fn escape_rbsp(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 8);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Removes emulation prevention bytes, turning a NAL payload into an RBSP.
pub fn unescape_rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut i = 0;
    while i < nal.len() {
        if i + 2 < nal.len() && nal[i] == 0 && nal[i + 1] == 0 && nal[i + 2] == 3 {
            out.push(0);
            out.push(0);
            i += 3;
        } else {
            out.push(nal[i]);
            i += 1;
        }
    }
    out
}

pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn u(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            if self.pos >= self.data.len() * 8 {
                return None;
            }
            let bit = (self.data[self.pos >> 3] >> (7 - (self.pos & 7))) & 1;
            v = (v << 1) | bit as u32;
            self.pos += 1;
        }
        Some(v)
    }

    pub fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.u(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        if zeros == 0 {
            return Some(0);
        }
        Some((1 << zeros) - 1 + self.u(zeros)?)
    }
}
