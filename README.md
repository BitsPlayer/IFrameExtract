# iframe-extract

Recovers I-frames from MP4 and QuickTime MOV files, H.264 or H.265, as images, by carving the
bitstream directly. It never reads the `moov` atom — not even when one is present.

That constraint is the point. A truncated MP4 has lost `moov`, and with it both the sample index
*and* the `avcC`/`hvcC` box holding the parameter sets, so no ordinary demuxer can decode a single frame.
This tool reconstructs both from the bytes that survive.

Scope: MP4 or MOV / H.264 or H.265 / known resolution. Interleaved audio needs `--with-audio`.

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
| `--codec auto\|h264\|h265` | defaults to detecting the codec from the bitstream |
| `--with-audio` | step over interleaved audio chunks in `mdat` |
| `--all-frames` | decode every picture, not just keyframes (uses ffmpeg) |
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

**MOV.** QuickTime and MP4 share the box layout the carver relies on, so a `.mov` is walked the
same way: `wide` in place of `free`, and pre-`ftyp` files that open straight on `wide` or `mdat`.
The report records which container the `ftyp` brand names.

## H.265

**Detection.** Nothing outside `moov` names the codec, so both NAL grammars are probed against
`mdat`, and the one most confidently made of coded slices wins (the lower confidence bound of the
slice share, so a pure but tiny run of fabricated units cannot beat a truncated file's few real
pictures). HEVC's two-byte NAL header is
far more redundant than H.264's (layer id 0, temporal id non-zero, IRAP pictures on the base
sub-layer), which makes it reject padding even more reliably. Dolby Vision RPUs and the second
view of MV-HEVC spatial video are accepted in the chain and left out of decoding.

**Parameter sets.** In-band VPS/SPS/PPS win when the encoder repeated them (Intel/AMD VAAPI do).
Otherwise there is far less to go on than with x264: ffmpeg's libx265 wrapper puts the options SEI
in `hvcC` with the parameter sets, so a truncated file keeps neither. Worse, ffmpeg's HEVC decoder
renders a wrong guess as a plausible picture without any error, so it cannot judge candidates
the way openh264 does for H.264.

So candidates are proven rather than guessed. `hevc::validate` runs the full CABAC syntax of an
intra slice — SAO, coding quadtree, intra modes, transform tree, residuals — without
reconstructing anything. Every field that changes how a bin is decoded (CTB and block sizes,
transform depth, SAO, sign hiding, cu_qp_delta, transform skip, WPP, bit depth, and the QP that
seeds the contexts) desynchronises the arithmetic decoder when wrong, and a desynchronised decoder
does not finish each substream exactly on its stop bit. The right set always does.

The search tries x265's presets, Apple VideoToolbox (iPhone) and Intel's hardware encoder first, each across every `init_qp`,
then a staged sweep: slice-header layouts the keyframes admit, then slice-data fields in order of
how common they are, on every core. The chosen set is then re-checked against every keyframe, and
any that disagree join the probes. Fields no parse can observe (strong intra smoothing, scaling
lists, chroma QP offsets, inter-only settings) are taken from the nearest known encoder.

Real iPhone recordings (iPhone X on iOS 13, iPhone 12 mini on iOS 16; HEVC with LPCM audio and
metadata tracks interleaved) recover every keyframe byte-identical to ffmpeg's decode, intact and
truncated. Note that ffmpeg applies the MOV clean aperture and rotation by default; compare with
`-apply_cropping codec -noautorotate` to see the coded picture this tool writes.

**Decode.** ffmpeg, as for `--all-frames` on H.264; openh264 has no HEVC support.

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

`tests/hevc.rs` covers H.265 and MOV without the corpus, on files it encodes with ffmpeg:
keyframes byte-identical to ffmpeg's decode of the intact file, a truncated MOV with AAC, a
configuration only the sweep finds, codec detection, and the validator rejecting each wrong field.

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
