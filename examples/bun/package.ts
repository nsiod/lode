// package.ts — bundle app.ts into ONE self-contained file: dist/app.js.
//
//   bun run package.ts            # version baked as 0.0.0-dev
//   bun run package.ts 1.2.3      # bake version 1.2.3 into the artifact
//
// dist/app.js is the release artifact lode installs (asset = "app.js"); lode runs
// it with `run = "bun run"`. Bundling means there is no node_modules to ship and
// no `bun install` at deploy time — exactly what the lode runtime model wants.

const version = Bun.argv[2] ?? "0.0.0-dev";

const result = await Bun.build({
  entrypoints: ["app.ts"],
  outdir: "dist",
  target: "bun", // produced JS expects the Bun runtime (lode runs it via `bun run`)
  // A single entry with no dynamic import() yields one file: dist/app.js.
  // Inline the baked version so the bundle self-reports it when run standalone.
  define: { BUILD_VERSION: JSON.stringify(version) },
});

if (!result.success) {
  for (const message of result.logs) console.error(message);
  process.exit(1);
}

console.log(`built dist/app.js (version ${version}, ${result.outputs.length} file)`);
