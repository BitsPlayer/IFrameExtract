# iframe-extract

Recovers pictures from an MP4 by carving the H.264 bitstream in `mdat`. It does not read `moov`, even when that box is still in the file.

A truncated MP4 has lost the sample index and the `avcC` box that held SPS and PPS. An ordinary demuxer then cannot decode a frame. This tool rebuilds both from the bytes that survived the cut.

BitsPlayer calls this library for the Extract tab. The `iframe-extract` binary is the same carve, driven from the command line.

## Recover frames

![From a file and a picture size to frames named by byte offset](docs/diagrams/session.svg)

```bash
cargo run --release -- clip.mp4 --size 1280x536 -o out/
```

`--size` is required. Resolution is taken from the arguments, because a missing SPS cannot be trusted to supply it. `--matrix` chooses BT.709 or BT.601 for PNG colour; that value lived in the SPS VUI.

Stderr shows a progress bar for each phase. `-q` silences it.

| Flag | Effect |
| --- | --- |
| `--size WxH` | Picture size. Required. |
| `-o DIR` | Output directory. Defaults to `out/`. |
| `--format png\|yuv` | `yuv` is planar 4:2:0, for a byte-exact compare against another decoder. |
| `--matrix bt709\|bt601` | Colour matrix for PNG. Defaults to BT.709. |
| `--include-non-idr` | Also emit intra slices that are not IDR. |
| `--include-partial` | Also attempt the frame that straddles the cut. |
| `--with-audio` | Step over non-video regions when an audio track is interleaved in `mdat`. |
| `--all-frames` | Also decode P-frames, and remux `recovered.mp4` with ffmpeg. |
| `--mp4` | Remux the recovered Annex B stream to `recovered.mp4`. Needs ffmpeg. |
| `--json FILE` | Machine-readable report. |
| `--emit-annexb FILE` | Annex B path. Defaults to `<output>/recovered.264`. |
| `--no-annexb` | Skip the Annex B file. |
| `-q` | No progress bar or summary. |

Pictures are named `frame_00007_off_0000151793.png`. A partial frame adds `_partial`. There are no timestamps: the timescale lived in `moov`, and the frame rate lived in the SPS VUI. The byte offset is stable.

## How the program is put together

![CLI, extract use case, domain parsers, and the openh264 decoder](docs/diagrams/layers.svg)

| Layer | Role |
| --- | --- |
| `src/presentation/` | The CLI: flags, file read, PNG/YUV/Annex B/JSON/MP4 write. |
| `src/application/` | `extract_with_decoder`. Orders the carve and talks to the decoder port. |
| `src/domain/` | `mdat` range, NAL walk, x264 SEI, SPS/PPS synthesis, slice headers, Annex B. No decoder crate and no filesystem. |
| `src/infrastructure/` | openh264 behind `FrameDecoder` / `VideoDecoder`. |

A second decoder can implement the same port. The carver does not change.

## The carve

![Find mdat, walk AVCC, recover parameter sets, group access units, decode](docs/diagrams/pipeline.svg)

**Find `mdat`, ignore everything else.** Declared box sizes are clamped to the file length. A truncated file's `mdat` header still advertises its original size.

**Walk the AVCC chain.** Samples are stored as a 4-byte length and then a NAL. Walking that chain from the start of `mdat` rebuilds frame boundaries without an index. The chain stops at the first unit that fails validation. With `--with-audio`, a second pass steps over interleaved non-video regions.

Padding after the cut tries hard to look like data, so units are checked on several axes: the `forbidden_zero_bit`, a whitelist of the NAL types x264 emits, the `nal_ref_idc` values the spec fixes for each type, a plausibility bound on length, and a slice-header check on the macroblock address. Together these reject `startcodes`, `fakeboxes`, and `mp4tail` padding.

**Recover the parameter sets.** In-band SPS/PPS are used when the encoder repeated them. Otherwise they are synthesised from `--size` plus x264's options SEI, which is the first NAL of a typical file and therefore survives a tail cut:

```text
cabac=1 ref=3 8x8dct=1 chroma_qp_offset=-2 bframes=3 keyint=48 rc=crf crf=23.0 ...
```

The remaining fields follow x264's header writer. `log2_max_frame_num` is 4 for ordinary GOPs. `pic_order_cnt_type` is 0 (lsb 6) unless there are no B-frames, in which case it is 2. DPB size uses x264's max(ref, 1+reorder, pyramid?4:1) rule. Scaling matrices are absent. `gaps_in_frame_num_value_allowed` is 0. On a baseline encode the synthesised PPS comes out byte-identical to the real one.

`pic_init_qp` is the one field the SEI cannot pin down. Plain CRF often matches `round(crf)`, but `--stitchable` and ABR leave x264's PPS default of 26. The tool tries the SEI hint first and validates each candidate by decoding the first IDR. CABAC context initialisation depends on SliceQPY, so a wrong guess desynchronises the decoder and fails the decode. If that still fails, a short fallback tries the other common entropy, transform, and POC flavours so a missing or non-x264 SEI is not fatal.

**Group access units.** A VCL NAL with `first_mb_in_slice == 0` starts a new picture, so multi-slice encodes group without access-unit delimiters.

A picture is trusted only once the chain has moved on to the next picture. Without that rule a multi-slice frame whose last slice fell past the cut looks whole, because every surviving slice is individually well formed.

**Decode.** openh264. By default only IDR pictures are decoded. `--include-non-idr` adds other intra slices. `--all-frames` also decodes P-frames and, with ffmpeg on the path, remuxes `recovered.mp4`.

## Check a corpus

```bash
cargo build --release
python3 scripts/evaluate.py
python3 scripts/evaluate.py --png-ssim 6
cargo test --release
```

The smoke tests skip themselves when the corpus is absent. Comparison is done on planar YUV. Both decoders are conformant, so the pass criterion is byte identity. PNG output differs from ffmpeg's PNGs by about 0.998 mean SSIM; that gap is the YUV-to-RGB step, where two implementations disagree by under one LSB per channel.

## Limits

- Head truncation is out of scope. The tool needs `ftyp` and `mdat` at the start, and the SEI as the first NAL when the parameter sets have to be synthesised.
- `--size` is required, and `--matrix` is a choice you supply. Both lived in structures a tail cut can destroy.
- There are no timestamps, for the same reason.
- A partial frame is reported. With `--include-partial` the decoder may still refuse a truncated IDR. openh264 does not emit a picture for a slice-incomplete IDR.
- The synthesis table follows x264. Other encoders use the search path: the opposite entropy coder, 8×8 transform, POC type, and every `pic_init_qp`. That covers typical High, Main, and Baseline IDRs. It will not invent a scaling matrix or an interlaced SPS.
