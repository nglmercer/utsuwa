import test from 'node:test';
import assert from 'node:assert/strict';

import { detectExplicitAvatarCommand } from './avatar-commands.ts';

test('detects English avatar commands', () => {
	assert.deepEqual(detectExplicitAvatarCommand('Jump!'), { type: 'animation', action: 'jump' });
	assert.deepEqual(detectExplicitAvatarCommand('can you jump?'), { type: 'animation', action: 'jump' });
	assert.deepEqual(detectExplicitAvatarCommand('Wave at me'), { type: 'animation', action: 'wave' });
	assert.deepEqual(detectExplicitAvatarCommand('nod if you agree'), { type: 'animation', action: 'nod' });
	assert.deepEqual(detectExplicitAvatarCommand('shake your head'), {
		type: 'animation',
		action: 'shake_head'
	});
	assert.deepEqual(detectExplicitAvatarCommand('take a bow'), { type: 'animation', action: 'bow' });
	assert.deepEqual(detectExplicitAvatarCommand('dance for me'), { type: 'animation', action: 'dance' });
	assert.deepEqual(detectExplicitAvatarCommand("let's celebrate!"), {
		type: 'animation',
		action: 'celebrate'
	});
	assert.deepEqual(detectExplicitAvatarCommand('just shrug it off'), {
		type: 'animation',
		action: 'shrug'
	});
});

test('detects walk directions, defaulting bare walk to forward', () => {
	assert.deepEqual(detectExplicitAvatarCommand('walk left'), {
		type: 'locomotion',
		action: 'walk',
		direction: 'left',
		durationMs: 1200
	});
	assert.deepEqual(detectExplicitAvatarCommand('Walk Right!'), {
		type: 'locomotion',
		action: 'walk',
		direction: 'right',
		durationMs: 1200
	});
	assert.deepEqual(detectExplicitAvatarCommand('walk forward'), {
		type: 'locomotion',
		action: 'walk',
		direction: 'forward',
		durationMs: 1200
	});
	assert.deepEqual(detectExplicitAvatarCommand('step back'), {
		type: 'locomotion',
		action: 'walk',
		direction: 'back',
		durationMs: 1200
	});
	assert.deepEqual(detectExplicitAvatarCommand('walk'), {
		type: 'locomotion',
		action: 'walk',
		direction: 'forward',
		durationMs: 1200
	});
});

test('detects Japanese avatar commands', () => {
	assert.deepEqual(detectExplicitAvatarCommand('ジャンプして！'), { type: 'animation', action: 'jump' });
	assert.deepEqual(detectExplicitAvatarCommand('手を振って'), { type: 'animation', action: 'wave' });
	assert.deepEqual(detectExplicitAvatarCommand('踊って'), { type: 'animation', action: 'dance' });
	assert.deepEqual(detectExplicitAvatarCommand('左に歩いて'), {
		type: 'locomotion',
		action: 'walk',
		direction: 'left',
		durationMs: 1200
	});
	assert.deepEqual(detectExplicitAvatarCommand('お辞儀して'), { type: 'animation', action: 'bow' });
});

test('ignores ordinary chat and near-miss words', () => {
	for (const text of [
		'How was your day?',
		'The jumper cables are in the car',
		'She is a beautiful dancer',
		'the microwave beeped',
		'I walked to school',
		'',
		'bowling night was great'
	]) {
		assert.equal(detectExplicitAvatarCommand(text), null, text);
	}
	assert.equal(detectExplicitAvatarCommand(null as unknown as string), null);
});
