import test from 'node:test';
import assert from 'node:assert/strict';
import { parseLMStudioCapabilities, parseLMStudioModelCapabilities } from './model-capabilities.ts';

test('LM Studio trained_for_tool_use maps to native tool support', () => {
	assert.deepEqual(parseLMStudioCapabilities({ vision: true, trained_for_tool_use: true }), {
		vision: true,
		toolCalling: true,
		nativeToolCalling: true,
		toolCallingSupport: 'native'
	});
});

test('missing LM Studio capabilities stay unknown', () => {
	assert.deepEqual(parseLMStudioCapabilities({}), { toolCallingSupport: 'unknown' });
});

test('explicitly unsupported tool metadata is not treated as a routing failure', () => {
	assert.equal(parseLMStudioCapabilities({ trained_for_tool_use: false }).toolCallingSupport, 'unsupported');
});

test('legacy LM Studio capability arrays are supported', () => {
	assert.equal(parseLMStudioCapabilities(['vision', 'tool_use']).toolCallingSupport, 'native');
	assert.equal(
		parseLMStudioCapabilities(['vision', 'trained_for_tool_use']).toolCallingSupport,
		'native'
	);
	assert.equal(parseLMStudioCapabilities(['vision']).vision, true);
});

test('root-level legacy capability fields are accepted', () => {
	assert.equal(
		parseLMStudioModelCapabilities({ id: 'model', trained_for_tool_use: true }).toolCallingSupport,
		'native'
	);
});
