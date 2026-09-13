import { cloudflareTest } from "@cloudflare/vitest-plugin";
import { readD1Migrations } from "@cloudflare/vitest-plugin";
import { defineConfig } from "vitest/config";

export default defineConfig(async () => ({
  plugins: [
    cloudflareTest({
      wrangler: { configPath: "./wrangler.local.jsonc" },
      miniflare: {
        bindings: {
          TEST_MIGRATIONS: await readD1Migrations("migrations"),
          MAX_RECORDS: 3,
        },
      },
    }),
  ],
  test: {
    // Custom suffix keeps these workerd-only tests out of the app's Vitest suite.
    include: ["tests/**/*.worker.ts"],
  },
}));
