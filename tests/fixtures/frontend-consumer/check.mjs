import { loadResources } from './resources.mjs';
console.log(JSON.stringify({ status: 'consumer candidate loaded; provenance must be established by caller', hashes: (await loadResources()).hashes }, null, 2));
