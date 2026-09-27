#!/usr/bin/env python3
import argparse
import hashlib
import json
import pathlib


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--lock", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()

    lock_path = pathlib.Path(args.lock)
    lock_digest = hashlib.sha256(lock_path.read_bytes()).hexdigest()
    package = {
        "name": "lingxi-local-app-template",
        "SPDXID": "SPDXRef-Pnpm-Lockfile",
        "versionInfo": "pnpm-lock.yaml",
        "downloadLocation": "NOASSERTION",
        "filesAnalyzed": False,
        "licenseConcluded": "NOASSERTION",
        "licenseDeclared": "NOASSERTION",
        "checksums": [{"algorithm": "SHA256", "checksumValue": lock_digest}],
        "sourceInfo": f"pnpm lockfile sha256: {lock_digest}",
    }
    document = {
        "spdxVersion": "SPDX-2.3",
        "dataLicense": "CC0-1.0",
        "SPDXID": "SPDXRef-DOCUMENT",
        "name": "lingxi-local-app-runtime",
        "documentNamespace": f"https://lingxi.app/spdx/local-app-runtime/{lock_digest}",
        "creationInfo": {
            "created": "1970-01-01T00:00:00Z",
            "creators": ["Organization: LingXi", "Tool: generate-local-app-sbom.py"],
        },
        "packages": [package],
        "relationships": [
            {
                "spdxElementId": "SPDXRef-DOCUMENT",
                "relationshipType": "DESCRIBES",
                "relatedSpdxElement": package["SPDXID"],
            }
        ],
    }
    output = pathlib.Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(document, indent=2, sort_keys=False) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
