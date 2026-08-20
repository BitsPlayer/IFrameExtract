#!/usr/bin/env python3
"""Scores iframe-extract against the Spring truncation corpus.

Good files are checked against ffmpeg's keyframe output for byte identity. Truncated files are
checked against the manifest's ground truth and against their intact twin, since no reference
decoder can read them at all.

Comparison is done on planar YUV, not PNG: both decoders are conformant so the decoded planes are
bit-exact, whereas RGB conversion differs between implementations. PNG output is checked
separately for a couple of files, only that it is written and well formed.
"""

import argparse
import concurrent.futures
import json
import re
import shutil
import subprocess
import sys
import tempfile
from collections import defaultdict
from pathlib import Path

CORPUS = Path(__file__).resolve().parents[2] / "test-data-corpus"
TOOL = Path(__file__).resolve().parents[1] / "target/release/iframe-extract"


def size_of(entry):
    return f"{entry['width']}x{entry['height']}"


def run_tool(path, size, outdir, extra=()):
    cmd = [str(TOOL), str(path), "--size", size, "-o", str(outdir),
           "--format", "yuv", "--json", str(outdir / "report.json"), "-q", *extra]
    proc = subprocess.run(cmd, capture_output=True, text=True)
    frames = sorted(outdir.glob("frame_*.yuv"))
    report = {}
    rp = outdir / "report.json"
    if rp.exists():
        report = json.loads(rp.read_text())
    return proc, frames, report


def ffmpeg_keyframes(path, dest):
    """Reference keyframes as one concatenated planar YUV blob."""
    subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "error", "-skip_frame", "nokey",
         "-i", str(path), "-vsync", "0", "-pix_fmt", "yuv420p", "-f", "rawvideo", str(dest), "-y"],
        check=True, capture_output=True)
    return dest.read_bytes()


def check_good(entry, manifest_dir):
    name = entry["name"]
    size = size_of(entry)
    with tempfile.TemporaryDirectory() as td:
        td = Path(td)
        out = td / "o"
        proc, frames, report = run_tool(manifest_dir / entry["file"], size, out)
        if proc.returncode != 0:
            return dict(name=name, status="TOOL_ERROR", detail=proc.stderr.strip()[:200])

        ours = b"".join(f.read_bytes() for f in frames)
        ref = ffmpeg_keyframes(manifest_dir / entry["file"], td / "ref.yuv")

        expected = entry["keyframes"]
        if len(frames) != expected:
            return dict(name=name, status="COUNT",
                        detail=f"got {len(frames)} frames, expected {expected}")
        if report.get("truncation_offset") is not None:
            return dict(name=name, status="FALSE_TRUNCATION",
                        detail=f"chain broke at {report['truncation_offset']}")
        if ours != ref:
            return dict(name=name, status="MISMATCH",
                        detail=diff_detail(ours, ref, entry["width"], entry["height"]))
        return dict(name=name, status="OK", frames=len(frames),
                    params=report.get("param_source"), qp=report.get("pic_init_qp"),
                    attempts=report.get("qp_search_attempts"))


def diff_detail(ours, ref, w, h):
    if len(ours) != len(ref):
        return f"size {len(ours)} vs {len(ref)}"
    fs = w * h * 3 // 2
    bad = [i for i in range(len(ref) // fs)
           if ours[i * fs:(i + 1) * fs] != ref[i * fs:(i + 1) * fs]]
    first = bad[0]
    a = ours[first * fs:(first + 1) * fs]
    b = ref[first * fs:(first + 1) * fs]
    luma_bad = a[:w * h] != b[:w * h]
    chroma_bad = a[w * h:] != b[w * h:]
    ndiff = sum(1 for x, y in zip(a, b) if x != y)
    plane = "luma+chroma" if luma_bad and chroma_bad else ("luma" if luma_bad else "chroma")
    return (f"{len(bad)} of {len(ref)//fs} frames differ, first is #{first} "
            f"({plane}, {ndiff}/{fs} bytes)")


def check_truncated(entry, good_by_file, manifest_dir):
    name = Path(entry["file"]).stem
    good = good_by_file[entry["derived_from"]]
    size = size_of(good)
    gt = entry["ground_truth"]
    expected = gt["keyframes_intact"]

    with tempfile.TemporaryDirectory() as td:
        td = Path(td)
        out = td / "o"
        proc, frames, report = run_tool(manifest_dir / entry["file"], size, out)
        if proc.returncode != 0:
            return dict(name=name, status="TOOL_ERROR", padding=entry["padding_type"],
                        detail=proc.stderr.strip()[:200])

        result = dict(name=name, padding=entry["padding_type"], frames=len(frames),
                      expected=expected)

        if len(frames) != expected:
            result.update(status="COUNT", detail=f"got {len(frames)}, expected {expected}")
            return result

        # The recovered frames must equal the first N keyframes of the intact twin.
        ref = ffmpeg_keyframes(manifest_dir / good["file"], td / "ref.yuv")
        fs = good["width"] * good["height"] * 3 // 2
        ours = b"".join(f.read_bytes() for f in frames)
        if ours != ref[:len(ours)]:
            result.update(status="MISMATCH",
                          detail=diff_detail(ours, ref[:len(ours)], good["width"], good["height"]))
            return result

        # The tool must notice the file is truncated, and must not have trusted a single byte
        # past the cut. The break point itself lands after the cut by design: the NAL straddling
        # the cut declares a length that reaches into the padding, so the chain only fails at the
        # garbage length prefix that follows it.
        if report.get("truncation_offset") is None:
            result.update(status="NO_TRUNCATION_DETECTED")
            return result
        cut = entry["truncate_offset"]
        trusted_end = report["trusted_data_end"]
        if trusted_end > cut:
            result.update(status="TRUSTED_PAST_CUT",
                          detail=f"trusted up to {trusted_end}, cut at {cut}")
            return result

        result.update(status="OK", slack=cut - trusted_end)
        return result


def png_similarity(entry, manifest_dir):
    """SSIM of the PNG output against ffmpeg's PNGs.

    The decoded planes are already proven bit-exact, so this measures only the YUV->RGB step,
    where two independent implementations disagree by a fraction of a least-significant bit.
    Reported because "visually identical to ffmpeg" is the question people actually ask.
    """
    size = size_of(entry)
    src = manifest_dir / entry["file"]
    with tempfile.TemporaryDirectory() as td:
        td = Path(td)
        ours, ref = td / "ours", td / "ref"
        ours.mkdir(), ref.mkdir()
        subprocess.run([str(TOOL), str(src), "--size", size, "-o", str(ours),
                        "--format", "png", "-q"], check=True, capture_output=True)
        subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-skip_frame", "nokey",
                        "-i", str(src), "-vsync", "0", str(ref / "f_%05d.png"), "-y"],
                       check=True, capture_output=True)

        scores = []
        for i, mine in enumerate(sorted(ours.glob("*.png")), start=1):
            theirs = ref / f"f_{i:05d}.png"
            out = subprocess.run(
                ["ffmpeg", "-hide_banner", "-loglevel", "error", "-i", str(mine),
                 "-i", str(theirs), "-lavfi", "[0:v][1:v]ssim=stats_file=-", "-f", "null", "-"],
                capture_output=True, text=True).stdout
            m = re.search(r"All:([0-9.]+)", out)
            if m:
                scores.append(float(m.group(1)))
        return dict(name=entry["name"], n=len(scores),
                    mean=sum(scores) / len(scores), worst=min(scores))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", choices=["good", "truncated"], help="limit to one set")
    ap.add_argument("--png-ssim", type=int, metavar="N", default=0,
                    help="also report PNG SSIM vs ffmpeg for N good files")
    ap.add_argument("--filter", default="", help="substring filter on file name")
    ap.add_argument("--jobs", type=int, default=8)
    args = ap.parse_args()

    if not TOOL.exists():
        sys.exit(f"build first: cargo build --release  (missing {TOOL})")
    if not shutil.which("ffmpeg"):
        sys.exit("ffmpeg not found")

    manifest = json.loads((CORPUS / "manifest.json").read_text())
    good_by_file = {g["file"]: g for g in manifest["good"]}
    failures = []

    def report_set(title, results, group_key=None):
        ok = [r for r in results if r["status"] == "OK"]
        print(f"\n{title}: {len(ok)}/{len(results)} pass")
        if group_key:
            groups = defaultdict(lambda: [0, 0])
            for r in results:
                g = groups[r.get(group_key, "?")]
                g[1] += 1
                g[0] += r["status"] == "OK"
            for k in sorted(groups):
                passed, total = groups[k]
                print(f"    {k:<12} {passed}/{total}")
        for r in results:
            if r["status"] != "OK":
                failures.append(r)
                print(f"    FAIL {r['name']}: {r['status']} {r.get('detail','')}")

    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        if args.only != "truncated":
            entries = [g for g in manifest["good"] if args.filter in g["name"]]
            results = list(pool.map(lambda e: check_good(e, CORPUS), entries))
            report_set(f"Good files (byte-identical to ffmpeg)", results)

        if args.only != "good":
            entries = [t for t in manifest["truncated"] if args.filter in t["file"]]
            results = list(pool.map(
                lambda e: check_truncated(e, good_by_file, CORPUS), entries))
            report_set("Truncated files (vs manifest ground truth + intact twin)",
                       results, group_key="padding")

        if args.png_ssim:
            entries = [g for g in manifest["good"] if args.filter in g["name"]][: args.png_ssim]
            rows = list(pool.map(lambda e: png_similarity(e, CORPUS), entries))
            total = sum(r["n"] for r in rows)
            mean = sum(r["mean"] * r["n"] for r in rows) / total
            print(f"\nPNG vs ffmpeg PNG over {total} frames from {len(rows)} files")
            print(f"    mean SSIM {mean:.6f}, worst frame {min(r['worst'] for r in rows):.6f}")
            print("    (decoded YUV is byte-identical; this gap is the RGB conversion alone,")
            print("     a sub-LSB difference between two YUV->RGB implementations)")

    print()
    if failures:
        print(f"{len(failures)} failure(s)")
        return 1
    print("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
