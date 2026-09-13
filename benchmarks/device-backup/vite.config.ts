import { readFileSync } from "node:fs";
import path from "node:path";
import { defineConfig, mergeConfig } from "vite";
import product from "../../vite.config";

export default defineConfig(async (environment) => {
  if (environment.mode !== "agent")
    throw new Error("Synthetic device smoke requires agent mode");
  const root = path.resolve(import.meta.dirname, "../..");
  const directory = path.join(root, "benchmarks/device-backup");
  const base =
    typeof product === "function" ? await product(environment) : product;
  return mergeConfig(base, {
    root,
    plugins: [
      {
        name: "device-smoke-entry",
        enforce: "pre",
        transformIndexHtml: {
          order: "pre",
          handler: () =>
            readFileSync(path.join(directory, "index.html"), "utf8"),
        },
      },
    ],
    build: {
      outDir: path.join(directory, "dist"),
      emptyOutDir: true,
      rolldownOptions: { input: path.join(root, "index.html") },
    },
  });
});
