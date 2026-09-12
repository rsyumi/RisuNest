import { execFileSync, spawn } from "node:child_process";
import { resolve } from "node:path";

// Verification-only launcher for fresh synthetic data, never installed profiles.
export function syntheticDaemon(dataDirectory, port) {
  const linux = process.argv.includes("--linux-server");
  const unixPath = (path) => {
    const absolute = resolve(path).replaceAll("\\", "/");
    if (!/^[A-Za-z]:\//.test(absolute))
      throw new Error("Expected Windows drive path");
    return `/mnt/${absolute[0].toLowerCase()}/${absolute.slice(3)}`;
  };
  const target = process.env.CARGO_TARGET_DIR;
  if (!target) throw new Error("Shared CARGO_TARGET_DIR is required");
  const executable = linux
    ? unixPath(
        target + "/x86_64-unknown-linux-gnu/release/risunest-sync-server",
      )
    : resolve(target, "debug/risunest-sync-server.exe");
  const data = linux ? unixPath(dataDirectory) : dataDirectory;
  const command = linux ? "wsl.exe" : executable;
  const prefix = linux ? ["-d", "Ubuntu-24.04", "--exec", executable] : [];
  return {
    platform: linux ? "linux-x86_64-wsl2" : "windows-x86_64",
    cli: (...args) =>
      execFileSync(command, [...prefix, ...args, "--data-dir", data], {
        windowsHide: true,
        encoding: "utf8",
        timeout: 15000,
        stdio: ["ignore", "pipe", "pipe"],
      }),
    async start() {
      // Closing stdin on completion or parent loss terminates this owned child.
      // Do not kill wsl.exe and leave its daemon running in the distribution.
      const shell =
        '"$1" serve --data-dir "$2" --listen "$3" & child=$!; trap \'kill -TERM "$child" 2>/dev/null; wait "$child"\' EXIT; read -r stop';
      const child = linux
        ? spawn(
            "wsl.exe",
            [
              "-d",
              "Ubuntu-24.04",
              "--exec",
              "/bin/bash",
              "-c",
              shell,
              "synthetic-server",
              executable,
              data,
              `127.0.0.1:${port}`,
            ],
            { windowsHide: true, stdio: ["pipe", "ignore", "pipe"] },
          )
        : spawn(
            executable,
            ["serve", "--data-dir", data, "--listen", `127.0.0.1:${port}`],
            { windowsHide: true, stdio: ["ignore", "ignore", "pipe"] },
          );
      await new Promise((resolve, reject) => {
        let output = "";
        const timer = setTimeout(() => {
          if (linux) child.stdin.end("stop\n");
          else child.kill();
          reject(new Error("Synthetic daemon readiness timeout"));
        }, 15000);
        child.once("error", (error) => {
          clearTimeout(timer);
          reject(error);
        });
        child.once("exit", (code) => {
          clearTimeout(timer);
          reject(new Error(`Synthetic daemon exited ${code}: ${output}`));
        });
        child.stderr.on("data", (bytes) => {
          output = (output + bytes.toString()).slice(-2048);
          if (output.includes("sync listener ready:")) {
            clearTimeout(timer);
            resolve();
          }
        });
      });
      return {
        async stop() {
          if (child.exitCode !== null || child.signalCode !== null) return;
          const exited = new Promise((resolve) => child.once("exit", resolve));
          if (linux) child.stdin.end("stop\n");
          else child.kill();
          let timer;
          try {
            await Promise.race([
              exited,
              new Promise((_, reject) => {
                timer = setTimeout(
                  () => reject(new Error("Synthetic daemon did not stop")),
                  10000,
                );
              }),
            ]);
          } finally {
            clearTimeout(timer);
          }
        },
      };
    },
  };
}
