import { readFile } from 'node:fs/promises';
import { resolve, isAbsolute } from 'node:path';
import { createHash } from 'node:crypto';
import assert from 'node:assert/strict';

export const paths = ['messages/en/common.json', 'messages/fr/common.json'];
export const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
export function expectedTarget(value) {
  if (typeof value === 'string') return value.trim() ? `FR: ${value}` : value;
  if (Array.isArray(value)) return value.map(expectedTarget);
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([key, child]) => [key, expectedTarget(child)]));
  return value;
}

export async function loadResources(root = process.env.FANI_CONSUMER_ROOT) {
  if (!root || !isAbsolute(root)) throw new Error('FANI_CONSUMER_ROOT must be an absolute external candidate directory');
  const bytes = await Promise.all(paths.map(path => readFile(resolve(root, path))));
  const [source, target] = bytes.map(value => JSON.parse(value));
  const seed = JSON.parse(await readFile(new URL('./seed/messages/en/common.json', import.meta.url), 'utf8'));
  assert.deepEqual(source, seed, 'Candidate source must match the fixture seed');
  assert.deepEqual(target, expectedTarget(source), 'Candidate target must match the recorded FR-prefix provider convention');
  const hashes = Object.fromEntries(paths.map((path, index) => [path, sha256(bytes[index])]));
  if (process.env.FANI_CONSUMER_HASHES) {
    const expected = JSON.parse(await readFile(process.env.FANI_CONSUMER_HASHES, 'utf8'));
    assert.deepEqual(hashes, expected, 'Candidate SHA-256 mismatch');
  }
  return { resources: { en: { common: source }, fr: { common: target } }, hashes };
}
