#!/usr/bin/env python3
"""Generate the Homebrew formula for a released lmdeck version.

    scripts/gen-homebrew-formula.py --version 0.1.0 --checksums checksums.txt > Formula/lmdeck.rb

The checksums file is the release asset produced by the release workflow:
one `sha256  lmdeck-<version>-<target>.tar.gz` line per platform.
"""

import argparse
import re
import sys

TARGETS = {
    ("macos", "arm"): "aarch64-apple-darwin",
    ("macos", "intel"): "x86_64-apple-darwin",
    ("linux", "arm"): "aarch64-unknown-linux-gnu",
    ("linux", "intel"): "x86_64-unknown-linux-gnu",
}

FORMULA = """\
class Lmdeck < Formula
  desc "Run local LLM recipes: quant, engine, hardware, and multi-node"
  homepage "https://github.com/{repo}"
  version "{version}"
  license "MIT"

  on_macos do
{macos}
  end

  on_linux do
{linux}
  end

  def install
    bin.install "lmdeck"
  end

  test do
    assert_match "lmdeck {version}", shell_output("#{{bin}}/lmdeck --version")
  end
end
"""


def read_sums(path, version):
    sums = {}
    for line in open(path, encoding="utf-8"):
        parts = line.split()
        if len(parts) != 2:
            continue
        digest, name = parts
        prefix = f"lmdeck-{version}-"
        if not (name.startswith(prefix) and name.endswith(".tar.gz")):
            continue
        target = name[len(prefix) : -len(".tar.gz")]
        if not re.fullmatch(r"[0-9a-f]{64}", digest):
            sys.exit(f"invalid sha256 for {name}: {digest}")
        sums[target] = digest
    return sums


def block(arch, url, digest, indent):
    pad = " " * indent
    return f'{pad}on_{arch} do\n{pad}  url "{url}"\n{pad}  sha256 "{digest}"\n{pad}end\n'


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--version", required=True, help="release version, with or without the v prefix")
    ap.add_argument("--checksums", required=True, help="checksums.txt from the release")
    ap.add_argument("--repo", default="javierbits/lmdeck", help="owner/name on GitHub")
    args = ap.parse_args()

    version = args.version[1:] if args.version.startswith("v") else args.version
    sums = read_sums(args.checksums, version)

    missing = [t for t in TARGETS.values() if t not in sums]
    if missing:
        sys.exit("missing checksums for: " + ", ".join(missing))

    def platform(prefix):
        return "".join(
            block(
                arch,
                f"https://github.com/{args.repo}/releases/download/v{version}/lmdeck-{version}-{target}.tar.gz",
                sums[target],
                4,
            )
            for (os_name, arch), target in TARGETS.items()
            if os_name == prefix
        )

    sys.stdout.write(
        FORMULA.format(
            repo=args.repo,
            version=version,
            macos=platform("macos").rstrip("\n"),
            linux=platform("linux").rstrip("\n"),
        )
    )


if __name__ == "__main__":
    main()
