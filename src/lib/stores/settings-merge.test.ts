import test from 'node:test';
import assert from 'node:assert/strict';

import { mergePendingSecrets } from './settings-merge.ts';

interface Cfg {
	apiKey?: string;
	baseUrl?: string;
}
interface Srv {
	id: string;
	transport: string;
	url?: string;
}

test('locked-time provider edits win per provider, others preserved', () => {
	const merged = mergePendingSecrets<Cfg, Srv>(
		{
			providerConfigs: {
				openai: { apiKey: 'old' },
				keep: { apiKey: 'untouched' }
			},
			mcpServers: []
		},
		{
			providerConfigs: { openai: { apiKey: 'new' } },
			mcpServers: []
		}
	);
	assert.deepEqual(merged.providerConfigs, {
		openai: { apiKey: 'new' },
		keep: { apiKey: 'untouched' }
	});
});

test('locked-time servers overlay by id and append new ones', () => {
	const merged = mergePendingSecrets<Cfg, Srv>(
		{
			providerConfigs: {},
			mcpServers: [
				{ id: 'a', transport: 'http', url: 'https://a-old.example' },
				{ id: 'b', transport: 'http', url: 'https://b.example' }
			]
		},
		{
			providerConfigs: {},
			mcpServers: [
				{ id: 'a', transport: 'http', url: 'https://a-new.example' },
				{ id: 'c', transport: 'http', url: 'https://c.example' }
			]
		}
	);
	assert.deepEqual(merged.mcpServers, [
		{ id: 'a', transport: 'http', url: 'https://a-new.example' },
		{ id: 'b', transport: 'http', url: 'https://b.example' },
		{ id: 'c', transport: 'http', url: 'https://c.example' }
	]);
});

test('empty pending edits leave decrypted secrets unchanged', () => {
	const decrypted = {
		providerConfigs: { openai: { apiKey: 'k' } },
		mcpServers: [{ id: 'a', transport: 'http' }]
	};
	const merged = mergePendingSecrets<Cfg, Srv>(decrypted, { providerConfigs: {}, mcpServers: [] });
	assert.deepEqual(merged, decrypted);
});
