import test from 'node:test';
import assert from 'node:assert/strict';

import { createTurnWatchdog } from './turn-watchdog.ts';

const tick = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

test('hard budget fires when the turn never finishes', async () => {
	const messages: string[] = [];
	const watchdog = createTurnWatchdog({
		hardTimeoutMs: 20,
		progressTimeoutMs: 1000,
		onTimeout: (message) => messages.push(message)
	});
	watchdog.start();
	await tick(50);
	assert.equal(messages.length, 1);
	assert.match(messages[0], /timed out/);
});

test('progress re-arms the no-progress budget, not the hard budget', async () => {
	const messages: string[] = [];
	const watchdog = createTurnWatchdog({
		hardTimeoutMs: 90,
		progressTimeoutMs: 25,
		onTimeout: (message) => messages.push(message)
	});
	watchdog.start();
	await tick(10);
	watchdog.progress();
	await tick(10);
	watchdog.progress();
	// No-progress never fired despite exceeding 25ms total.
	assert.deepEqual(messages, []);
	// The hard budget was NOT re-armed by progress: it still fires ~90ms
	// after start (not 90ms after the last progress call). The idle
	// no-progress budget fires first along the way.
	await tick(55);
	assert.equal(messages.length, 1);
	assert.match(messages[0], /no progress/);
	await tick(25);
	assert.equal(messages.length, 2);
	assert.match(messages[1], /timed out/);
});

test('silence fires the no-progress budget', async () => {
	const messages: string[] = [];
	const watchdog = createTurnWatchdog({
		hardTimeoutMs: 1000,
		progressTimeoutMs: 20,
		onTimeout: (message) => messages.push(message)
	});
	watchdog.start();
	await tick(50);
	assert.equal(messages.length, 1);
	assert.match(messages[0], /no progress/);
});

test('pause suspends both budgets, progress re-arms after', async () => {
	const messages: string[] = [];
	const watchdog = createTurnWatchdog({
		hardTimeoutMs: 30,
		progressTimeoutMs: 20,
		onTimeout: (message) => messages.push(message)
	});
	watchdog.start();
	watchdog.pause();
	await tick(60);
	assert.deepEqual(messages, []);
	// Post-resume progress re-arms both budgets; continued silence fires
	// the no-progress budget first, then the hard budget.
	watchdog.progress();
	await tick(15);
	assert.deepEqual(messages, []);
	await tick(40);
	assert.equal(messages.length, 2);
	assert.match(messages[0], /no progress/);
	assert.match(messages[1], /timed out/);
});

test('stop disarms everything', async () => {
	const messages: string[] = [];
	const watchdog = createTurnWatchdog({
		hardTimeoutMs: 10,
		progressTimeoutMs: 10,
		onTimeout: (message) => messages.push(message)
	});
	watchdog.start();
	watchdog.stop();
	await tick(40);
	assert.deepEqual(messages, []);
});
