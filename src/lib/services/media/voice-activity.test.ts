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

test('detects speech from a low-gain microphone', () => {
	const detector = new VoiceActivityDetector();
	detector.start(0);

	assert.equal(detector.update(0.006, 100), null);
	assert.equal(detector.update(0.025, 200), null);
	assert.equal(detector.update(0.025, 300), 'speech-start');
	assert.equal(detector.hasDetectedSpeech, true);
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

test('exposes raw VAD diagnostics without applying display amplification', () => {
	const detector = new VoiceActivityDetector();
	detector.start(0);

	detector.update(0.01, 100);
	const quiet = detector.getDiagnostics(100);
	assert.equal(quiet.currentRms, 0.01);
	assert.equal(quiet.peakRms, 0.01);
	assert.equal(quiet.speechThreshold, 0.015);
	assert.equal(quiet.speechDetected, false);

	assert.equal(detector.update(0.1, 200), null);
	const candidate = detector.getDiagnostics(200);
	assert.equal(candidate.currentRms, 0.1);
	assert.equal(candidate.peakRms, 0.1);
	assert.equal(candidate.speechCandidateActive, true);
	assert.equal(candidate.speechThreshold, 0.015);

	assert.equal(detector.update(0.1, 300), 'speech-start');
	const speaking = detector.getDiagnostics(300);
	assert.equal(speaking.speechDetected, true);
	assert.equal(speaking.vadEvent, 'speech-start');
});

test('adapts the noise floor and keeps the threshold above quiet noise', () => {
	const detector = new VoiceActivityDetector({ minSpeechRms: 0.01, noiseMultiplier: 2 });
	detector.start(0);

	detector.update(0.008, 100);
	const diagnostics = detector.getDiagnostics(100);
	assert.ok(diagnostics.noiseFloor > 0.005);
	assert.ok(diagnostics.speechThreshold >= 0.01);
});
