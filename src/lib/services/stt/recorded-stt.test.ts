import test from 'node:test';
import assert from 'node:assert/strict';

import { getAudioExtension } from './recorded-stt.ts';

test('derives recording filenames from the actual audio MIME type', () => {
	assert.equal(getAudioExtension('audio/webm;codecs=opus'), 'webm');
	assert.equal(getAudioExtension('audio/ogg;codecs=opus'), 'ogg');
	assert.equal(getAudioExtension('audio/mp4'), 'm4a');
	assert.equal(getAudioExtension('audio/mpeg'), 'mp3');
	assert.equal(getAudioExtension('audio/wav'), 'wav');
	assert.equal(getAudioExtension('audio/unknown'), 'webm');
});

