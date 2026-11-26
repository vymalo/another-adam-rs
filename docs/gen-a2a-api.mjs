// Writes api/a2a-server.md (not committed) from api/a2a-server.openapi.json: the operations the server serves,
// as a table. The JSON is the document the code builds (docs/reference/a2a-server.md, ANR-939).
import { readFileSync, writeFileSync } from "node:fs";

const doc = JSON.parse(readFileSync(new URL("./api/a2a-server.openapi.json", import.meta.url), "utf8"));
const cell = (s) => String(s ?? "").replace(/\|/g, "\\|").replace(/\s+/g, " ").trim();
const first = (s) => cell(String(s ?? "").split("\n")[0]);
const rows = [];
for (const [path, item] of Object.entries(doc.paths ?? {}))
  for (const [method, op] of Object.entries(item))
    if (["get", "put", "post", "delete", "patch"].includes(method))
      rows.push(`| \`${method.toUpperCase()}\` | \`${path}\` | \`${cell(op.operationId)}\` | ${first(op.summary ?? op.description)} |`);
writeFileSync(
  new URL("./api/a2a-server.md", import.meta.url),
  `# A2A server (OpenAPI)\n\n${cell(doc.info?.title)}, version ${cell(doc.info?.version)}. The contract is [a2a-server.openapi.json](a2a-server.openapi.json) (OpenAPI 3.1), committed from the code and held to it by a drift test. Its operations, read from it at build time:\n\n| Method | Path | Operation | Summary |\n|---|---|---|---|\n${rows.join("\n")}\n`,
);
