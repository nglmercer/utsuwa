// CI dependency-audit gate: fails on any NEW high/critical advisory.
// Accepted risks live in ALLOWLIST with a recorded reason + recheck date;
// everything else must be fixed (upgrade/override) before merge.
import { execFileSync } from 'node:child_process';

// protobufjs 6.11.4 via onnx-proto (onnxruntime-web, @xenova/transformers).
// onnx-proto pins protobufjs ^6, so 7.x cannot be reached without breaking
// onnxruntime-web; the pinned v2 transformers line has no newer release.
// Exposure is supply-chain-only: only first-party-pinned embedding models
// from HuggingFace are ever decoded — never user-supplied descriptors.
// Recheck: migrate embeddings to @huggingface/transformers (v3+) and drop.
const ALLOWLIST = new Set([
	'GHSA-xq3m-2v4x-88gg', // critical: code execution via untrusted descriptors
	'GHSA-66ff-xgx4-vchm',
	'GHSA-75px-5xx7-5xc7',
	'GHSA-jvwf-75h9-cwgg',
	'GHSA-685m-2w69-288q',
	'GHSA-wcpc-wj8m-hjx6'
]);

let raw;
try {
	raw = execFileSync('pnpm', ['audit', '--json'], { encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 });
} catch (error) {
	// pnpm audit exits non-zero when findings exist; the JSON still parses.
	raw = error.stdout ?? '{}';
}
const advisories = JSON.parse(raw).advisories ?? {};
const fresh = [];
for (const advisory of Object.values(advisories)) {
	if (advisory.severity !== 'high' && advisory.severity !== 'critical') continue;
	const id = String(advisory.url ?? '').split('/').pop();
	if (!id || ALLOWLIST.has(id)) continue;
	fresh.push(`${advisory.severity} ${id} (${advisory.module_name}): ${(advisory.title ?? '').slice(0, 100)}`);
}
if (fresh.length > 0) {
	console.error('New high/critical advisories (not in the audit allowlist):');
	for (const line of fresh) console.error(`  ${line}`);
	process.exit(1);
}
console.log('audit gate: no new high/critical advisories');
