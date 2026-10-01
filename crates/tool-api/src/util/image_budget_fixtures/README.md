# Image budget regression fixture

`large-baseline.jpg.gz` is an original synthetic 16000 × 12000 solid-gray
baseline JPEG, generated for this test using `generate-large-baseline.c` and
libjpeg at quality 80. No external image or personal data is included. The
JPEG is 3,000,625 bytes but expands to 576,000,000 RGB bytes when decoded at
full resolution; gzip keeps the checked-in fixture below 10 KB. Tests unzip
the JPEG and decode it using IDCT sampling, avoiding the full pixel allocation.

To regenerate with libjpeg installed:

```sh
cc generate-large-baseline.c -ljpeg -o /tmp/generate-large-baseline
/tmp/generate-large-baseline /tmp/large-baseline.jpg
python3 - <<'PY'
import gzip
from pathlib import Path
Path('large-baseline.jpg.gz').write_bytes(
    gzip.compress(Path('/tmp/large-baseline.jpg').read_bytes(), compresslevel=9, mtime=0)
)
PY
```

The JPEG byte stream can vary by libjpeg version, but dimensions and decoded
gray pixels should remain the same. The allocation-guard tests change frame
headers to advertise progressive/lossless coding or extreme dimensions, and
verify rejection before decoding scan data. These mutations are header-only
allocation probes, not complete valid images in those coding modes.
