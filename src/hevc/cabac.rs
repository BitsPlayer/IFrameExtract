//! HEVC CABAC decoding engine and the intra-slice context set.
//!
//! Only what an I slice uses is here: the validator never parses inter slices, so the context
//! tables carry initType 0 values only.

/// Context indices into `Contexts::states`, one block per syntax element.
pub mod ctx {
    pub const SAO_MERGE: usize = 0;
    pub const SAO_TYPE: usize = 1;
    pub const SPLIT_CU: usize = 2; // 3
    pub const TRANSQUANT_BYPASS: usize = 5;
    pub const CU_QP_DELTA: usize = 6; // 2
    pub const PART_MODE: usize = 8;
    pub const PREV_INTRA_LUMA: usize = 9;
    pub const CHROMA_PRED: usize = 10;
    pub const SPLIT_TRANSFORM: usize = 11; // 3
    pub const CBF_LUMA: usize = 14; // 2
    pub const CBF_CHROMA: usize = 16; // 5
    pub const TRANSFORM_SKIP: usize = 21; // 2
    pub const LAST_X: usize = 23; // 18
    pub const LAST_Y: usize = 41; // 18
    pub const CODED_SUB_BLOCK: usize = 59; // 4
    pub const SIG_COEFF: usize = 63; // 44
    pub const GREATER1: usize = 107; // 24
    pub const GREATER2: usize = 131; // 6
    pub const COUNT: usize = 137;
}

/// initType 0 (I slice) initialisation values, laid out to match `ctx`.
const INIT_VALUES: [u8; ctx::COUNT] = [
    153, // sao_merge_left/up_flag
    200, // sao_type_idx
    139, 141, 157, // split_cu_flag
    154, // cu_transquant_bypass_flag
    154, 154, // cu_qp_delta_abs
    184, // part_mode
    184, // prev_intra_luma_pred_flag
    63,  // intra_chroma_pred_mode
    153, 138, 138, // split_transform_flag
    111, 141, // cbf_luma
    94, 138, 182, 154, 154, // cbf_cb / cbf_cr
    139, 139, // transform_skip_flag
    // last_sig_coeff_x_prefix
    110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111, 143, 127, 111, 79, 108, 123, 63,
    // last_sig_coeff_y_prefix
    110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111, 143, 127, 111, 79, 108, 123, 63, 91, 171,
    134, 141, // coded_sub_block_flag
    // sig_coeff_flag
    111, 111, 125, 110, 110, 94, 124, 108, 124, 107, 125, 141, 179, 153, 125, 107, 125, 141, 179,
    153, 125, 107, 125, 141, 179, 153, 125, 140, 139, 182, 182, 152, 136, 152, 136, 153, 136, 139,
    111, 136, 139, 111, 141, 111, // coeff_abs_level_greater1_flag
    140, 92, 137, 138, 140, 152, 138, 139, 153, 74, 149, 92, 139, 107, 122, 152, 140, 179, 166,
    182, 140, 227, 122, 197, // coeff_abs_level_greater2_flag
    138, 153, 136, 167, 152, 152,
];

const RANGE_TAB_LPS: [[u8; 4]; 64] = [
    [128, 176, 208, 240],
    [128, 167, 197, 227],
    [128, 158, 187, 216],
    [123, 150, 178, 205],
    [116, 142, 169, 195],
    [111, 135, 160, 185],
    [105, 128, 152, 175],
    [100, 122, 144, 166],
    [95, 116, 137, 158],
    [90, 110, 130, 150],
    [85, 104, 123, 142],
    [81, 99, 117, 135],
    [77, 94, 111, 128],
    [73, 89, 105, 122],
    [69, 85, 100, 116],
    [66, 80, 95, 110],
    [62, 76, 90, 104],
    [59, 72, 86, 99],
    [56, 69, 81, 94],
    [53, 65, 77, 89],
    [51, 62, 73, 85],
    [48, 59, 69, 80],
    [46, 56, 66, 76],
    [43, 53, 63, 72],
    [41, 50, 59, 69],
    [39, 48, 56, 65],
    [37, 45, 54, 62],
    [35, 43, 51, 59],
    [33, 41, 48, 56],
    [32, 39, 46, 53],
    [30, 37, 43, 50],
    [29, 35, 41, 48],
    [27, 33, 39, 45],
    [26, 31, 37, 43],
    [24, 30, 35, 41],
    [23, 28, 33, 39],
    [22, 27, 32, 37],
    [21, 26, 30, 35],
    [20, 24, 29, 33],
    [19, 23, 27, 31],
    [18, 22, 26, 30],
    [17, 21, 25, 28],
    [16, 20, 23, 27],
    [15, 19, 22, 25],
    [14, 18, 21, 24],
    [14, 17, 20, 23],
    [13, 16, 19, 22],
    [12, 15, 18, 21],
    [12, 14, 17, 20],
    [11, 14, 16, 19],
    [11, 13, 15, 18],
    [10, 12, 15, 17],
    [10, 12, 14, 16],
    [9, 11, 13, 15],
    [9, 11, 12, 14],
    [8, 10, 12, 14],
    [8, 9, 11, 13],
    [7, 9, 11, 12],
    [7, 9, 10, 12],
    [7, 8, 10, 11],
    [6, 8, 9, 11],
    [6, 7, 9, 10],
    [6, 7, 8, 9],
    [2, 2, 2, 2],
];

const TRANS_IDX_LPS: [u8; 64] = [
    0, 0, 1, 2, 2, 4, 4, 5, 6, 7, 8, 9, 9, 11, 11, 12, 13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21,
    21, 22, 22, 23, 24, 24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33, 33, 33, 34,
    34, 35, 35, 35, 36, 36, 36, 37, 37, 37, 38, 38, 63,
];

#[derive(Clone, Copy, Default)]
pub struct State {
    p_state: u8,
    val_mps: u8,
}

#[derive(Clone)]
pub struct Contexts {
    pub states: [State; ctx::COUNT],
}

impl Contexts {
    pub fn new(slice_qp: i32) -> Self {
        let qp = slice_qp.clamp(0, 51);
        let mut states = [State::default(); ctx::COUNT];
        for (s, &init) in states.iter_mut().zip(INIT_VALUES.iter()) {
            let slope = (init >> 4) as i32;
            let offset = (init & 15) as i32;
            let m = slope * 5 - 45;
            let n = (offset << 3) - 16;
            let pre = (((m * qp) >> 4) + n).clamp(1, 126);
            if pre <= 63 {
                *s = State {
                    p_state: (63 - pre) as u8,
                    val_mps: 0,
                };
            } else {
                *s = State {
                    p_state: (pre - 64) as u8,
                    val_mps: 1,
                };
            }
        }
        Self { states }
    }
}

/// Arithmetic decoder over an RBSP, tracking the exact bit position so that the end of a
/// substream can be checked against its stop bit.
pub struct Engine<'a> {
    data: &'a [u8],
    /// Index of the next bit to read.
    pos: usize,
    /// Bits past `end_bit` count as overrun: a desynchronised parse runs off the end of its data.
    end_bit: usize,
    range: u32,
    offset: u32,
    pub overrun: usize,
}

impl<'a> Engine<'a> {
    /// Starts decoding at byte `start`, treating `end` (exclusive) as the end of the substream.
    pub fn new(data: &'a [u8], start: usize, end: usize) -> Option<Self> {
        let mut e = Self {
            data,
            pos: start * 8,
            end_bit: end.min(data.len()) * 8,
            range: 510,
            offset: 0,
            overrun: 0,
        };
        e.offset = e.bits(9);
        // 510 and 511 are forbidden initial offsets.
        (e.offset < 510).then_some(e)
    }

    pub fn bit_pos(&self) -> usize {
        self.pos
    }

    fn bit(&mut self) -> u32 {
        let b = if self.pos < self.end_bit {
            ((self.data[self.pos >> 3] >> (7 - (self.pos & 7))) & 1) as u32
        } else {
            self.overrun += 1;
            0
        };
        self.pos += 1;
        b
    }

    fn bits(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit();
        }
        v
    }

    pub fn decision(&mut self, s: &mut State) -> u32 {
        let q = ((self.range >> 6) & 3) as usize;
        let lps = RANGE_TAB_LPS[s.p_state as usize][q] as u32;
        self.range -= lps;
        let bin;
        if self.offset >= self.range {
            bin = 1 - s.val_mps as u32;
            self.offset -= self.range;
            self.range = lps;
            if s.p_state == 0 {
                s.val_mps = 1 - s.val_mps;
            }
            s.p_state = TRANS_IDX_LPS[s.p_state as usize];
        } else {
            bin = s.val_mps as u32;
            s.p_state = (s.p_state + 1).min(62);
        }
        while self.range < 256 {
            self.range <<= 1;
            self.offset = (self.offset << 1) | self.bit();
        }
        bin
    }

    pub fn bypass(&mut self) -> u32 {
        self.offset = (self.offset << 1) | self.bit();
        if self.offset >= self.range {
            self.offset -= self.range;
            1
        } else {
            0
        }
    }

    pub fn bypass_bits(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bypass();
        }
        v
    }

    pub fn terminate(&mut self) -> u32 {
        self.range -= 2;
        if self.offset >= self.range {
            1
        } else {
            while self.range < 256 {
                self.range <<= 1;
                self.offset = (self.offset << 1) | self.bit();
            }
            0
        }
    }
}
