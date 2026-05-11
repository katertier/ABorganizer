# Test fixtures

> Audio fixtures for integration + bench tests. Not committed to git
> (see `.gitignore`); built reproducibly from LibriVox sources +
> synthesized publisher jingles.

## Goals

- Cover end-to-end pipeline scenarios with public-domain audio
- Test Audible-style edge cases (publisher jingle, brand intro/outro,
  embedded ASIN tags) without copyright concerns
- Reproducible from sources via `scripts/build-fixtures.py` (TBD)

## Fixture set (planned)

Each fixture is a base LibriVox recording with synthesized
publisher prepending. We add fake MP4 metadata (ASIN, brand-intro
duration, publisher name) so the catalog stages have realistic
inputs.

| Fixture | Base (LibriVox) | Length | Has jingle | Multi-file | Notes |
|---|---|---|---|---|---|
| short | TBD | ~10 min | yes | no | Quick test loop |
| medium | TBD | ~3 hr | yes | no | Typical book |
| long | TBD | ~15 hr | yes | no | Large index test |
| multi | TBD | ~5 hr | yes | 5 MP3s | Multi-file boundary handling |
| no-jingle | TBD | ~30 min | no | no | Tier 0 reject path |
| german | TBD (German LibriVox) | ~1 hr | no | no | NLLanguageRecognizer test |
| chapters | TBD | ~2 hr | yes | no | Embedded chpl atoms |
| no-tags | TBD | ~1 hr | no | no | Pure filename heuristics |

## License

LibriVox recordings are public domain (or CC0 per recording —
check each `LICENSE.txt`). Synthesized jingles are generated locally
(tone sweeps + macOS `say` command). The composite fixtures are
themselves CC0.

## Build

```bash
# Once scripts/build-fixtures.py exists (v0.2-ish):
python3 scripts/build-fixtures.py --out tests/fixtures/
```

CI downloads + builds once, caches by checksum.

## Checksums

`tests/fixtures/manifest.toml` (committed) lists the expected SHA-256
of each built fixture. The build script verifies before write.
