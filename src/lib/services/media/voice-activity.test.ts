import test from 'node:test';
import assert from 'node:assert/strict';

import {
	calculateRms,
	DEFAULT_INITIAL_SILENCE_MS,
	DEFAULT_MAX_RECORDING_MS,
	DEFAULT_SPEECH_END_SILENCE_MS,
	VoiceActivityDetector
} from './voice-activity.ts';

test('calculates normalized time-domain RMS energy', () => {
	assert.equal(calculateRms(new Uint8Array([128, 128, 128])), 0);
	assert.ok(calculateRms(new Uint8Array([0, 255])) > 0.99);
});

test('requires confirmed speech before starting the silence window', () => {
	const detector = new VoiceActivityDetector();
	detector.start(0);

	assert.equal(detector.update(0.01, 100), null);
	assert.equal(detector.update(0.2, 200), null);
	assert.equal(detector.update(0.2, 299), null);
	assert.equal(detector.update(0.2, 300), 'speech-start');
	assert.equal(detector.update(0.01, 700), null);
	assert.equal(detector.update(0.01, 1_699), null);
	assert.equal(detector.update(0.01, 1_700), 'speech-end');
});

test('short pauses do not end an utterance', () => {
	const detector = new VoiceActivityDetector();
	detector.start(0);
	detector.update(0.2, 0);
	assert.equal(detector.update(0.2, 100), 'speech-start');

	assert.equal(detector.update(0.01, 200), null);
	assert.equal(detector.update(0.01, 600), null);
	assert.equal(detector.update(0.2, 700), null);
	assert.equal(detector.update(0.01, 800), null);
	assert.equal(detector.update(0.01, 800 + DEFAULT_SPEECH_END_SILENCE_MS - 1), null);
	assert.equal(detector.update(0.01, 800 + DEFAULT_SPEECH_END_SILENCE_MS), 'speech-end');
});

test('initial silence ends without pretending an utterance happened', () => {
	const detector = new VoiceActivityDetector();
	detector.start(0);
	assert.equal(detector.update(0, DEFAULT_INITIAL_SILENCE_MS), 'initial-silence');
	assert.equal(detector.hasDetectedSpeech, false);
});

test('maximum duration is a hard guardrail', () => {
	const detector = new VoiceActivityDetector({ initialSilenceMs: DEFAULT_MAX_RECORDING_MS + 1_000 });
	detector.start(0);
	assert.equal(detector.update(0, DEFAULT_MAX_RECORDING_MS), 'maximum-duration');
});
