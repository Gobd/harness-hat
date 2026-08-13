#!/usr/bin/env node

import fs from 'node:fs/promises';
import path from 'node:path';

const DEFAULT_OUTPUT_FILE = 'src-and-guides.md';
const TARGETS = ['src', 'User Guide', 'docker', 'Cargo.toml'];
const KNOWN_TEXT_EXTENSIONS = new Set([
  '.rs',
  '.toml',
  '.md',
  '.markdown',
  '.yml',
  '.yaml',
  '.json',
  '.js',
  '.mjs',
  '.ts',
  '.py',
  '.sh',
  '.lock',
  '.dockerfile',
  '.cfg',
  '.ini',
  '.txt',
  '.html',
  '.css',
  '.sql',
  '.c',
  '.cpp',
  '.h',
  '.hpp',
]);
const KNOWN_TEXT_BASENAMES = new Set([
  'dockerfile',
  'license',
  'readme',
  'changelog',
  'makefile',
]);

const args = process.argv.slice(2);
const outputArgIndex = args.indexOf('--output');
const outputFile =
  outputArgIndex !== -1 && args[outputArgIndex + 1]
    ? args[outputArgIndex + 1]
    : (args[0] && !args[0].startsWith('--') ? args[0] : DEFAULT_OUTPUT_FILE);

function languageForFile(filePath) {
  const base = path.basename(filePath).toLowerCase();
  const ext = path.extname(base).toLowerCase();

  if (base === 'dockerfile' || base.endsWith('.dockerfile')) {
    return 'dockerfile';
  }
  if (ext === '.rs') return 'rust';
  if (ext === '.toml') return 'toml';
  if (ext === '.py') return 'python';
  if (ext === '.js' || ext === '.mjs') return 'javascript';
  if (ext === '.ts') return 'typescript';
  if (ext === '.sh') return 'bash';
  if (ext === '.md' || ext === '.markdown') return 'markdown';
  if (ext === '.json') return 'json';
  if (ext === '.yml' || ext === '.yaml') return 'yaml';
  return '';
}

function isBinary(buffer) {
  if (buffer.includes(0)) {
    return true;
  }
  return false;
}

function isKnownTextFile(filePath, buffer) {
  if (isBinary(buffer)) {
    return false;
  }
  const base = path.basename(filePath).toLowerCase();
  const ext = path.extname(base).toLowerCase();
  if (KNOWN_TEXT_EXTENSIONS.has(ext) || KNOWN_TEXT_BASENAMES.has(base)) {
    return true;
  }
  const sample = buffer.slice(0, 2048);
  let nonText = 0;
  for (const byte of sample) {
    if (byte === 9 || byte === 10 || byte === 13) continue;
    if (byte < 32 || byte > 126) {
      nonText += 1;
    }
  }
  return nonText > sample.length * 0.1;
}

async function walkDirectory(dir) {
  const entries = await fs.readdir(dir, { withFileTypes: true });
  const files = [];
  for (const entry of entries) {
    if (entry.isSymbolicLink()) continue;
    const fullPath = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      files.push(...(await walkDirectory(fullPath)));
    } else if (entry.isFile()) {
      files.push(fullPath);
    }
  }
  return files;
}

async function collectFiles(target) {
  const stat = await fs.stat(target);
  if (stat.isFile()) return [target];
  if (!stat.isDirectory()) return [];
  return walkDirectory(target);
}

async function readFileForMarkdown(filePath) {
  const data = await fs.readFile(filePath);
  const lang = languageForFile(filePath);
  const textFile = isKnownTextFile(filePath, data);
  if (!textFile && isBinary(data)) {
    return {
      filePath,
      kind: 'binary',
      content: data.toString('base64'),
      language: 'base64',
    };
  }
  return {
    filePath,
    kind: 'text',
    content: data.toString('utf8'),
    language: lang,
  };
}

async function main() {
  const root = process.cwd();
  const blocks = [];

  const gatheredFiles = [];
  for (const target of TARGETS) {
    const targetPath = path.join(root, target);
    const fileList = await collectFiles(targetPath);
    for (const file of fileList) {
      gatheredFiles.push(file);
    }
  }

  gatheredFiles.sort((a, b) => a.localeCompare(b));

  for (const filePath of gatheredFiles) {
    const result = await readFileForMarkdown(filePath);
    const relPath = path.relative(root, result.filePath);
    if (result.kind === 'binary') {
      blocks.push(`## ${relPath}\n\nBinary file (Base64):\n\n\`\`\`base64\n${result.content}\n\`\`\``);
    } else {
      const fenceLang = result.language ? result.language : 'text';
      blocks.push(`## ${relPath}\n\n\`\`\`${fenceLang}\n${result.content}\n\`\`\``);
    }
  }

  const output = `# Combined source/documentation/docker bundle\n\nGenerated from: ${TARGETS.map((target) => path.join('.', target)).join(', ')}\n\n${blocks.join('\n\n')}\n`;
  await fs.writeFile(path.join(root, outputFile), output, 'utf8');
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
