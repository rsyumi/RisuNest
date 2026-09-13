import path from "node:path";
import { readFileSync } from "node:fs";
import { defineConfig, mergeConfig, type Plugin } from "vite";
import productConfig from "../vite.config";

// Harnesses consume the product's compiler/plugins; product builds do not load this file.
export function harnessConfig(
  name: "streaming" | "tokenizer" | "linux" | "macos",
) {
  return defineConfig(async (environment) => {
    if (environment.mode !== "agent")
      throw new Error("Harnesses require --mode agent");
    const root = path.resolve(import.meta.dirname, "..");
    const directory = path.join(root, "benchmarks", name);
    const htmlEntry: Plugin = {
      name: "harness-html-entry",
      enforce: "pre",
      // Keep the normal HTML scaffolding for the full-app tokenizer benchmark.
      transformIndexHtml: {
        order: "pre",
        handler(html) {
          if (name !== "tokenizer")
            return readFileSync(path.join(directory, "index.html"), "utf8");
          if (!html.includes('src="/src/main.ts"'))
            throw new Error("Product HTML entry changed");
          return html.replace(
            'src="/src/main.ts"',
            'src="/benchmarks/tokenizer/main.ts"',
          );
        },
      },
    };
    const base =
      typeof productConfig === "function"
        ? await productConfig(environment)
        : productConfig;
    return mergeConfig(base, {
      root,
      plugins: [htmlEntry],
      build: {
        outDir: path.join(directory, "dist"),
        emptyOutDir: true,
        rolldownOptions: {
          input: path.join(root, "index.html"),
        },
      },
    });
  });
}
