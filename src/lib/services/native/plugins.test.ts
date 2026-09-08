import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import {
	isServing,
	listPlugins,
	managePlugin,
	parsePluginInfo,
	pluginMethod,
	type InvokeFn
} from './plugins.ts';

const RECORD = {
	id: 'echo',
	name: 'Echo',
	version: '0.1.0',
	trust: 'unsigned-wasm',
	state: 'enabled',
	tools: ['run']
};

describe('parsePluginInfo', () => {
	it('parses a full host record', () => {
		assert.deepEqual(parsePluginInfo(RECORD), RECORD);
	});

	it('rejects garbage without throwing', () => {
		for (const bad of [
			null,
			42,
			'echo',
			{ ...RECORD, tools: 'run' },
			{ ...RECORD, tools: [42] },
			{ ...RECORD, state: 7 },
			{}
		]) {
			assert.equal(parsePluginInfo(bad), null);
		}
	});
});

describe('isServing', () => {
	it('is true only for enabled plugins', () => {
		assert.equal(isServing({ ...RECORD, state: 'enabled' }), true);
		for (const state of ['loaded', 'disabled', 'discovered', 'failed', 'validated']) {
			assert.equal(isServing({ ...RECORD, state }), false);
		}
	});
});

describe('pluginMethod', () => {
	it('maps operations to IPC methods', () => {
		assert.equal(pluginMethod('enable'), 'plugin.enable');
		assert.equal(pluginMethod('disable'), 'plugin.disable');
		assert.equal(pluginMethod('update'), 'plugin.update');
		assert.equal(pluginMethod('remove'), 'plugin.remove');
	});
});

describe('listPlugins', () => {
	it('parses arrays and drops unrecognizable entries', async () => {
		const invoke: InvokeFn = async () => [RECORD, null, { id: 'x' }];
		assert.deepEqual(await listPlugins(invoke), [RECORD]);
	});

	it('rejects non-arrays so the panel can show the failure', async () => {
		const invoke: InvokeFn = async () => ({ ok: true });
		await assert.rejects(() => listPlugins(invoke));
	});
});

describe('managePlugin', () => {
	it('invokes the lifecycle method then refreshes the list', async () => {
		const calls: Array<[string, Record<string, unknown> | undefined]> = [];
		const invoke: InvokeFn = async (method, params) => {
			calls.push([method, params]);
			if (method === 'plugin.list') return [{ ...RECORD, state: 'disabled' }];
			return { ok: true };
		};
		const out = await managePlugin(invoke, 'disable', 'echo');
		assert.deepEqual(calls, [
			['plugin.disable', { id: 'echo' }],
			['plugin.list', {}]
		]);
		assert.deepEqual(out, [{ ...RECORD, state: 'disabled' }]);
	});
});
