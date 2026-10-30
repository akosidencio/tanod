#!/usr/bin/env python3
"""Verify the release image's OCI archive before, and as, it is published.

    verify-oci-image.py ARCHIVE [--expect-revision SHA] [--expect-inputs HASH]
                        [--expect-digest sha256:...] [--compare-docker IMAGE]
                        [--json OUT]

The release workflow builds the image once, into an OCI archive. The smoke test
runs that archive's runtime image, and the publish job pushes that archive with
digests preserved. This script is what ties the three together:

* the archive holds exactly one runtime image, for linux/amd64, under one image
  index whose digest is the digest the registry will report after the push;
* that index carries BuildKit provenance and an SBOM for that image (the
  workflow promises both);
* the image's revision and build-input labels match the source the workflow
  checked out;
* with --compare-docker, the image Docker ran has the same layer contents
  (diff ids), user, entrypoint and labels, so the image that was tested is the
  image that is pushed;
* with --expect-digest, the archive is the one the build job recorded, so a
  re-run of the publish job cannot pick up a different build.

Standard library only. Exits non-zero, naming every problem, on any mismatch.
"""
import argparse
import hashlib
import json
import subprocess
import sys
import tarfile

INDEX = "application/vnd.oci.image.index.v1+json"
MANIFEST = "application/vnd.oci.image.manifest.v1+json"
ATTESTATION = "attestation-manifest"
PROVENANCE = ("https://slsa.dev/provenance/v0.2", "https://slsa.dev/provenance/v1")
SBOM = "https://spdx.dev/Document"


class Archive:
    def __init__(self, path):
        self.tar = tarfile.open(path)

    def read(self, name):
        member = self.tar.extractfile(name)
        if member is None:
            raise SystemExit(f"{name} is missing from the archive")
        return member.read()

    def blob(self, digest):
        algorithm, hexdigest = digest.split(":", 1)
        data = self.read(f"blobs/{algorithm}/{hexdigest}")
        actual = "sha256:" + hashlib.sha256(data).hexdigest()
        if actual != digest:
            raise SystemExit(f"blob {digest} has content digest {actual}")
        return data


def inspect(path):
    archive = Archive(path)
    top = json.loads(archive.read("index.json"))
    if len(top.get("manifests", [])) != 1:
        raise SystemExit(f"index.json lists {len(top.get('manifests', []))} manifests, expected 1")
    descriptor = top["manifests"][0]
    if descriptor.get("mediaType") != INDEX:
        raise SystemExit(f"top-level manifest is {descriptor.get('mediaType')}, not an image index "
                         "(was the image built without provenance and SBOM?)")
    index_digest = descriptor["digest"]
    index = json.loads(archive.blob(index_digest))

    images, attestations = [], []
    for entry in index.get("manifests", []):
        annotations = entry.get("annotations", {})
        if annotations.get("vnd.docker.reference.type") == ATTESTATION:
            attestations.append(entry)
        else:
            images.append(entry)

    problems = []
    if len(images) != 1:
        problems.append(f"{len(images)} runtime images in the index, expected exactly 1")
    image = images[0] if images else {}
    platform = image.get("platform", {})
    if (platform.get("os"), platform.get("architecture")) != ("linux", "amd64"):
        problems.append(f"runtime image platform {platform}, expected linux/amd64")

    manifest = json.loads(archive.blob(image["digest"])) if image else {}
    config_digest = manifest.get("config", {}).get("digest", "")
    config = json.loads(archive.blob(config_digest)) if config_digest else {}

    predicates = []
    for entry in attestations:
        if entry["annotations"].get("vnd.docker.reference.digest") != image.get("digest"):
            problems.append(f"attestation {entry['digest']} does not refer to the runtime image")
            continue
        attestation = json.loads(archive.blob(entry["digest"]))
        for layer in attestation.get("layers", []):
            predicate_type = layer.get("annotations", {}).get("in-toto.io/predicate-type", "")
            statement = json.loads(archive.blob(layer["digest"]))
            predicates.append(predicate_type)
            if predicate_type in PROVENANCE:
                build_type = statement.get("predicate", {}).get("buildType") or \
                    statement.get("predicate", {}).get("buildDefinition", {}).get("buildType")
                if not build_type:
                    problems.append("provenance predicate has no buildType")
    if not any(p in PROVENANCE for p in predicates):
        problems.append("no SLSA provenance attestation for the runtime image")
    if SBOM not in predicates:
        problems.append("no SPDX SBOM attestation for the runtime image")

    image_config = config.get("config", {})
    return {
        "archive": path,
        "index_digest": index_digest,
        "image_manifest_digest": image.get("digest"),
        "config_digest": config_digest,
        "platform": f"{platform.get('os')}/{platform.get('architecture')}",
        "diff_ids": config.get("rootfs", {}).get("diff_ids", []),
        "user": image_config.get("User"),
        "entrypoint": image_config.get("Entrypoint"),
        "labels": image_config.get("Labels") or {},
        "attestation_predicates": sorted(set(predicates)),
    }, problems


def compare_docker(result, image):
    out = subprocess.run(["docker", "image", "inspect", image], capture_output=True, text=True)
    if out.returncode != 0:
        return [f"docker image inspect {image} failed: {out.stderr.strip()}"]
    loaded = json.loads(out.stdout)[0]
    problems = []
    if loaded["RootFS"].get("Layers", []) != result["diff_ids"]:
        problems.append("the loaded image's layers differ from the archive's")
    for field, key in (("User", "user"), ("Entrypoint", "entrypoint"), ("Labels", "labels")):
        if (loaded["Config"].get(field) or ({} if key == "labels" else None)) != result[key]:
            problems.append(f"the loaded image's {field} differs from the archive's")
    result["docker_image_id"] = loaded["Id"]
    return problems


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("archive")
    parser.add_argument("--expect-revision")
    parser.add_argument("--expect-inputs")
    parser.add_argument("--expect-digest")
    parser.add_argument("--compare-docker", metavar="IMAGE")
    parser.add_argument("--json", metavar="OUT")
    args = parser.parse_args()

    result, problems = inspect(args.archive)
    labels = result["labels"]
    if args.expect_revision and labels.get("org.opencontainers.image.revision") != args.expect_revision:
        problems.append(f"revision label {labels.get('org.opencontainers.image.revision')!r}, "
                        f"expected {args.expect_revision!r}")
    if args.expect_inputs and labels.get("io.tanod.build-inputs-sha256") != args.expect_inputs:
        problems.append(f"build-inputs label {labels.get('io.tanod.build-inputs-sha256')!r}, "
                        f"expected {args.expect_inputs!r}")
    if args.expect_digest and result["index_digest"] != args.expect_digest:
        problems.append(f"archive index digest {result['index_digest']}, expected {args.expect_digest}")
    if args.compare_docker:
        problems += compare_docker(result, args.compare_docker)

    result["problems"] = problems
    if args.json:
        with open(args.json, "w") as out:
            json.dump(result, out, indent=2)
    print(json.dumps({k: result[k] for k in ("index_digest", "image_manifest_digest",
                                             "config_digest", "platform",
                                             "attestation_predicates")}, indent=2))
    if problems:
        for problem in problems:
            print(f"::error::{problem}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
