import fs from 'node:fs';
import path from 'node:path';
import { SHA } from './gitfix.mjs';

const IMAGE_NAME = 'registry.example.test/tasks/img';
export const IMAGE = IMAGE_NAME;
export const DIGEST = `sha256:${SHA('a')}`;

export const TOML = `[agent]
network_mode = "no-network"

[verifier]
network_mode = "public"

[environment]
cpus = 1
memory_mb = 4096
`;

/** A well-formed task directory named `id` under `root`; `over` replaces or (null) removes files. */
export function makeTask(root, id, over = {}) {
  const files = {
    'instruction.md': 'Make the thing work.\n',
    'task.toml': TOML,
    'environment/Dockerfile': `FROM ${IMAGE_NAME}@${DIGEST}\n`,
    'tests/test.sh': '#!/bin/bash\necho 1 > /logs/verifier/reward.txt\n',
    'solution/solution.patch': 'diff --git a/x b/x\n',
    ...over,
  };
  for (const [rel, content] of Object.entries(files)) {
    if (content === null) continue;
    const p = path.join(root, id, rel);
    fs.mkdirSync(path.dirname(p), { recursive: true });
    fs.writeFileSync(p, content);
  }
  return path.join(root, id);
}

