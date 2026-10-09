// Optional compact tool for isolated demo workspaces. OpenCode owns execution.
import { tool } from "@opencode-ai/plugin";
import { execFile } from "node:child_process";
import { promisify } from "node:util";

const run = promisify(execFile);
export default tool({
  description: "Run a shell command in the fixture workspace and return its output.",
  args: {
    command: tool.schema.string(),
    description: tool.schema.string().optional(),
  },
  async execute(args, context) {
    try {
      const result = await run("/bin/bash", ["-lc", args.command], {
        cwd: context.directory,
        timeout: 30000,
        maxBuffer: 4 * 1024 * 1024,
        signal: context.abort,
      });
      return result.stdout + result.stderr;
    } catch (error: any) {
      return `exit ${error.code}:\n${error.stdout ?? ""}${error.stderr ?? error.message}`;
    }
  },
});
