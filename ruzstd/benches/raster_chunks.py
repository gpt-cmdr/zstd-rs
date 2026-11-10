#!/usr/bin/env python3
"""Cuts public test rasters into the chunk files that the `rasterbench` example reads.

    python ruzstd/benches/raster_chunks.py --download rasters chunks
    cargo run --release --example rasterbench -- chunks

Needs `rasterio` and `numpy`. For each raster `name.tif` in the source directory it writes, in the output
directory:

* `name.raw`: up to 24 tiles of 512 x 512 pixels, pixel-interleaved, one after another, the way a tiled
  GeoTIFF holds them before compression;
* `name_p2.raw` (integer samples) or `name_p3.raw` (floating point): the same tiles after the TIFF
  predictor that GDAL would apply (horizontal differencing, or the floating point predictor);
* `name.len`: the length of a tile in bytes.

With `--download` the rasters are fetched first if they are not there yet (about 80 MB). Keep both
directories out of the repository.
"""
import argparse
import hashlib
import os
import sys
import urllib.request

import numpy as np
import rasterio

SOURCES = {
    "usgs_dem": "https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/1/TIFF/current/n40w106/USGS_1_n40w106.tif",
    "srtm90": "https://github.com/opengeos/datasets/releases/download/raster/srtm90.tif",
    "landsat": "https://github.com/opengeos/datasets/releases/download/raster/landsat.tif",
}
TILE = 512
MAX_TILES = 24


def tiles(a):
    """The whole tiles of an array of (bands, height, width), each as (height, width, bands)."""
    _, h, w = a.shape
    out = []
    for y in range(0, h - TILE + 1, TILE):
        for x in range(0, w - TILE + 1, TILE):
            out.append(np.ascontiguousarray(np.moveaxis(a[:, y : y + TILE, x : x + TILE], 0, -1)))
    return out


def predictor2(t):
    """Horizontal differencing: each sample minus the sample of the same band to its left."""
    d = t.copy()
    d[:, 1:, :] = t[:, 1:, :] - t[:, :-1, :]
    return d


def predictor3(t):
    """The floating point predictor: the bytes of a row grouped by significance (most significant
    first), then each byte minus the byte one sample to its left."""
    h, w, b = t.shape
    size = t.dtype.itemsize
    by = t.astype(t.dtype.newbyteorder(">")).view(np.uint8).reshape(h, w * b, size)
    sh = np.concatenate([by[:, :, i] for i in range(size)], axis=1)
    d = sh.copy()
    d[:, b:] = sh[:, b:] - sh[:, :-b]
    return d


def download(name, path):
    print(f"downloading {SOURCES[name]}", flush=True)
    request = urllib.request.Request(SOURCES[name], headers={"User-Agent": "zstd-rust-rasterbench"})
    with urllib.request.urlopen(request) as response, open(path + ".part", "wb") as out:
        while True:
            block = response.read(1 << 20)
            if not block:
                break
            out.write(block)
    os.replace(path + ".part", path)


def write(path, data):
    with open(path, "wb") as f:
        f.write(data)
    print(f"  {os.path.basename(path):20} {len(data):9} bytes  sha256 {hashlib.sha256(data).hexdigest()[:16]}")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("source", help="directory of the rasters (name.tif)")
    parser.add_argument("output", help="directory to write the chunk files to")
    parser.add_argument("--download", action="store_true", help="fetch the rasters that are missing")
    parser.add_argument("names", nargs="*", default=list(SOURCES), help="rasters to cut (default: all)")
    args = parser.parse_args()
    os.makedirs(args.source, exist_ok=True)
    os.makedirs(args.output, exist_ok=True)
    for name in args.names:
        path = os.path.join(args.source, name + ".tif")
        if not os.path.exists(path):
            if not (args.download and name in SOURCES):
                sys.exit(f"{path} is missing (use --download for {', '.join(SOURCES)})")
            download(name, path)
        with rasterio.open(path) as ds:
            a = ds.read()
        print(f"{name}: {a.dtype} {a.shape}")
        ts = tiles(a)
        ts = ts[:: max(1, len(ts) // MAX_TILES)][:MAX_TILES]
        if not ts:
            sys.exit(f"{path} is smaller than a tile of {TILE} x {TILE}")
        out = os.path.join(args.output, name)
        write(out + ".raw", b"".join(t.tobytes() for t in ts))
        if a.dtype.kind == "f":
            write(out + "_p3.raw", b"".join(predictor3(t).tobytes() for t in ts))
        else:
            write(out + "_p2.raw", b"".join(predictor2(t).tobytes() for t in ts))
        with open(out + ".len", "w") as f:
            f.write(str(ts[0].nbytes))


if __name__ == "__main__":
    main()
