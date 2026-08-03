import { spawn } from "node:child_process"
import { mkdtemp, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import path from "node:path"

const repositoryRoot = path.resolve(import.meta.dirname, "../../../..")
const binary = path.join(
  repositoryRoot,
  "target",
  "debug",
  process.platform === "win32" ? "needle.exe" : "needle",
)
const dataDirectory = await mkdtemp(path.join(tmpdir(), "needle-playwright-"))
const server = spawn(binary, ["serve", "--data-dir", dataDirectory], {
  cwd: repositoryRoot,
  stdio: ["ignore", "pipe", "pipe"],
})

let stderr = ""
server.stderr.setEncoding("utf8")
server.stderr.on("data", (chunk) => {
  stderr += chunk
})

try {
  const launchUrl = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`Needle serve timed out: ${stderr}`)), 15_000)
    server.stdout.setEncoding("utf8")
    server.stdout.on("data", (chunk) => {
      const match = chunk.match(/Needle control plane: (http:\/\/[^\s]+)/)
      if (match) {
        clearTimeout(timer)
        resolve(match[1])
      }
    })
    server.once("exit", (code) => {
      clearTimeout(timer)
      reject(new Error(`Needle serve exited with ${code}: ${stderr}`))
    })
  })

  const playwrightCli = path.join(
    import.meta.dirname,
    "..",
    "node_modules",
    "@playwright",
    "test",
    "cli.js",
  )
  const status = await new Promise((resolve, reject) => {
    const tests = spawn(process.execPath, [playwrightCli, "test"], {
      cwd: path.resolve(import.meta.dirname, ".."),
      env: { ...process.env, NEEDLE_E2E_URL: launchUrl },
      stdio: "inherit",
    })
    tests.once("error", reject)
    tests.once("exit", resolve)
  })
  if (status !== 0) {
    process.exitCode = status ?? 1
  }
} finally {
  if (server.exitCode === null && server.signalCode === null) {
    server.kill()
    await new Promise((resolve) => {
      const timer = setTimeout(() => {
        server.kill("SIGKILL")
        resolve()
      }, 5_000)
      server.once("exit", () => {
        clearTimeout(timer)
        resolve()
      })
    })
  }
  await rm(dataDirectory, {
    recursive: true,
    force: true,
    maxRetries: 10,
    retryDelay: 100,
  })
}
