#!/usr/bin/env node
// Tiny dispatcher: probes each per-platform optional dependency, finds
// the one whose `os` / `cpu` constraints match the host (npm/bun installs
// exactly one of them — the rest silently fail and are skipped), then
// execs the native `swell` binary from that package.
//
// Platforms are kept in sync with the matrix in
// `.github/workflows/release.yml` — adding a target there means adding it
// to the optionalDependencies in package.json and to this list.

import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

const PLATFORM_PACKAGES = [
  "@dialo/swell-cli-linux-x64",
  "@dialo/swell-cli-linux-arm64",
  "@dialo/swell-cli-darwin-arm64",
];

let binaryPath;
for (const pkg of PLATFORM_PACKAGES) {
  try {
    binaryPath = require.resolve(`${pkg}/bin/swell`);
    break;
  } catch {
    // not installed on this host — try the next one
  }
}

if (!binaryPath) {
  console.error(
    `@dialo/swell-cli: no platform binary found for ${process.platform}/${process.arch}.\n` +
      `Expected one of:\n  ${PLATFORM_PACKAGES.join("\n  ")}\n` +
      `If you're on a supported platform, ensure your package manager installs ` +
      `optionalDependencies (npm/bun do by default).`,
  );
  process.exit(1);
}

const result = spawnSync(binaryPath, process.argv.slice(2), {
  stdio: "inherit",
});
process.exit(result.status ?? 1);
