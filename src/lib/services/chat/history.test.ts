import test from 'node:test';
import assert from 'node:assert/strict';
import { filterEmptyAssistantPlaceholders } from './history.ts';

test('empty assistant placeholders are omitted from provider history', () => {
	const history = filterEmptyAssistantPlaceholders([
		{ role: 'user', content: 'hello' },
		{ role: 'assistant', content: '' },
		{ role: 'user', content: 'read package.json' }
	]);

	assert.deepEqual(history, [
		{ role: 'user', content: 'hello' },
		{ role: 'user', content: 'read package.json' }
	]);
});

test('blank assistant messages containing tool calls are protocol-valid', () => {
	const toolMessage = {
		role: 'assistant',
		content: '',
		tool_calls: [{ id: 'call-1', name: 'filesystem.read' }]
	};
	assert.deepEqual(filterEmptyAssistantPlaceholders([toolMessage]), [toolMessage]);
});
