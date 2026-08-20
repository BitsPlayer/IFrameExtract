# iframe-extract

Recovers I-frames from MP4/H.264 files as images, by carving the bitstream directly. It never
reads the `moov` atom — not even when one is present.

That constraint is the point. A truncated MP4 has lost `moov`, and with it both the sample index
*and* the `avcC` box holding the SPS/PPS, so no ordinary demuxer can decode a single frame. This
tool reconstructs both from the bytes that survive.

Scope: MP4 / H.264 / no audio / known resolution.

## Usage

```
iframe-extract <input.mp4> --size 1280x536 -o out/
```

Stderr gets a pacman-style progress bar for each phase (read, walk, recover, decode, write).
`-q` silences it.

| flag | meaning |
|---|---|
| `--size WxH` | **required** — resolution cannot be recovered without an SPS |
| `--format png\|yuv` | `yuv` writes planar 4:2:0 for byte-exact comparison against a reference decoder |
| `--matrix bt709\|bt601` | colour matrix for PNG output; lives in the lost SPS VUI, so it must be told to us |
| `--include-non-idr` | also emit intra slices that are not IDR |
| `--include-partial` | also attempt the frame straddling the cut |
| `--json FILE` | machine-readable report |
| `--emit-annexb FILE` | recovered elementary stream, for cross-checking with other decoders |
| `-q` | no progress bar or summary |

Output is named `frame_00007_off_0000151793.png`. No timestamps: the timescale lives in `moov`
and the frame rate in the SPS VUI, both lost. The byte offset is stable and joins directly
against the corpus packet index.

## How it works

**Find `mdat`, ignore everything else.** Declared box sizes are clamped to the file length and
never trusted — a truncated file's `mdat` header still advertises its original size.

**Walk the AVCC chain.** Samples are stored as `[4-byte length][NAL]`, which is a self-describing
chain: walking it from the start of `mdat` reconstructs every frame boundary without an index.
The chain stops validating at the truncation point.

Padding after the cut tries hard to look like data, so units are validated on several axes: the
`forbidden_zero_bit`, a whitelist of the NAL types x264 emits, the `nal_ref_idc` values the spec
fixes for each type, a plausibility bound on length (no frame is many times its own uncompressed
size), and a slice-header sanity check on the macroblock address. Together these reject
`startcodes`, `fakeboxes` and `mp4tail` padding.

**Recover the parameter sets.** In-band SPS/PPS are used when the encoder repeated them.
Otherwise they are synthesised from `--size` plus x264's options SEI — which is the first NAL of
the file and therefore always survives a tail truncation:

```
cabac=1 ref=3 8x8dct=1 chroma_qp_offset=-2 bframes=3 keyint=48 rc=crf crf=23.0 ...
```

The remaining fields follow x264's header writer rather than a single corpus snapshot:
`log2_max_frame_num` is 4 for ordinary GOPs, `pic_order_cnt_type` is 0 (lsb 6) unless there are
no B-frames in which case it is 2, DPB size uses x264's max(ref, 1+reorder, pyramid?4:1) rule,
scaling matrices never appear, and `gaps_in_frame_num_value_allowed` is always 0. On the baseline
encode the synthesised PPS comes out byte-identical to the real one.

`pic_init_qp` is the one field the SEI cannot pin down. Plain CRF often matches `round(crf)`, but
`--stitchable` and ABR leave x264's PPS default of 26. The tool searches, trying the SEI hint
first, and validates each candidate by decoding the first IDR — CABAC context initialisation
depends on SliceQPY, so a wrong guess desynchronises the decoder and fails loudly rather than
silently. If that still fails, a short fallback tries the other common entropy / transform /
POC flavours so a missing or non-x264 SEI is not fatal.

**Group access units.** A VCL NAL with `first_mb_in_slice == 0` starts a new picture, so
multi-slice encodes group correctly without relying on access unit delimiters.

A picture is trusted only once the chain has moved on to the *next* picture. Without that rule a
multi-slice frame whose last slice fell past the cut looks whole, because every surviving slice is
individually well formed — nothing but the arrival of the next picture proves the previous one was
fully received.

**Decode.** openh264, behind a `FrameDecoder` trait so a hand-written Rust intra decoder can be
added later as a second backend without touching the carver.

## Verification

```
cargo build --release
python3 scripts/evaluate.py            # full corpus
python3 scripts/evaluate.py --png-ssim 6
cargo test --release                   # smoke tests, skipped if the corpus is absent
```

Against `../test-data-corpus` (264 files), current status:

| set | result |
|---|---|
| 66 intact files | **66/66** — every keyframe byte-identical to ffmpeg |
| 198 truncated files | **198/198** — frame count matches `keyframes_intact`, every frame byte-identical to its intact twin, and no byte past the cut ever trusted |

Truncated results hold across all six padding types (`none`, `zeros`, `random`, `startcodes`,
`fakeboxes`, `mp4tail`), 33/33 each.

Comparison is done on planar YUV rather than PNG. Both decoders are conformant, so decoded planes
are bit-exact and the pass criterion is byte identity, not a similarity threshold. PNG output
differs from ffmpeg's PNGs by ~0.998 mean SSIM; that gap is entirely the YUV→RGB step, where two
independent implementations disagree by under one LSB per channel (measured max 2–3 LSB, mean
below 1). The decoded image itself is exact.

## Limits

- **Head truncation is out of scope.** The tool needs `ftyp`/`mdat` at the start and the SEI as
  the first NAL.
- **`--size` is required**, and `--matrix` is a guess. Both live in structures the truncation
  destroyed.
- **No timestamps**, for the same reason.
- **Partial frames are reported, not invented.** When the cut lands inside an IDR, the frame is
  recorded in the JSON with `partial: true, decoded: false`, and no image is written. openh264
  will not emit a picture for a truncated or slice-incomplete IDR under any error-concealment
  mode — confirmed against its own `h264dec`, which also yields zero frames for a 2-of-4-slice
  IDR. Salvaging the macroblock rows that did survive needs a decoder that can emit a partial
  picture, which is the main thing the planned second backend would buy.
- **Non-IDR I-slices** yield nothing on this corpus, since x264 with `open_gop=0` does not emit
  them. The code path exists so the tool stays honest on arbitrary input.
- **The calibration table is x264-specific.** Other encoders fall back to the search path, which
  tries the opposite entropy coder, 8×8 transform, and POC type, plus every `pic_init_qp`. That
  covers typical High/Main/Baseline IDRs; it will not invent a scaling matrix or an interlaced
  SPS.
