import test from 'node:test';
import assert from 'node:assert/strict';

import {
	classifyMediaError,
	getMediaAccessErrorDetails,
	getMediaErrorDetails,
	getMediaErrorMessage
} from './media-errors.ts';

test('classifies standard media capture errors by device-independent category', () => {
	assert.equal(classifyMediaError(new DOMException('denied', 'NotAllowedError')), 'permission-denied');
	assert.equal(classifyMediaError({ name: 'SecurityError' }), 'permission-denied');
	assert.equal(classifyMediaError({ name: 'NotFoundError' }), 'not-found');
	assert.equal(classifyMediaError({ name: 'NotReadableError' }), 'busy');
	assert.equal(classifyMediaError({ name: 'AbortError' }), 'busy');
	assert.equal(classifyMediaError({ name: 'NotSupportedError' }), 'unsupported');
	assert.equal(classifyMediaError({ name: 'TypeError' }), 'unsupported');
	assert.equal(classifyMediaError({ name: 'OverconstrainedError' }), 'constraints');
	assert.equal(classifyMediaError({ name: 'ConstraintNotSatisfiedError' }), 'constraints');
	assert.equal(classifyMediaError({ name: 'UnknownError' }), 'unknown');
});

test('formats microphone and camera messages from the same categories', () => {
	assert.equal(
		getMediaErrorMessage('microphone', { name: 'NotAllowedError' }),
		'Microphone access was denied.'
	);
	assert.equal(
		getMediaErrorMessage('camera', { name: 'NotFoundError' }),
		'No camera found. Please connect a camera.'
	);
	assert.equal(
		getMediaErrorMessage('camera', { name: 'NotReadableError' }),
		'Camera is busy or in use by another app.'
	);
	assert.equal(
		getMediaErrorMessage('microphone', { name: 'OverconstrainedError' }),
		'Microphone does not meet requirements.'
	);
});

test('preserves useful messages for otherwise unknown errors', () => {
	assert.equal(
		getMediaErrorMessage('microphone', new Error('driver unavailable')),
		'Microphone error: driver unavailable'
	);
	assert.equal(getMediaErrorMessage('camera', {}), 'Failed to access camera');
});

test('reads error details across object realms', () => {
	assert.deepEqual(getMediaErrorDetails({ name: 'NotAllowedError', message: 'blocked' }), {
		name: 'NotAllowedError',
		message: 'blocked'
	});
	assert.deepEqual(getMediaErrorDetails(null), {});
});

test('includes page and API capability details in a media diagnostic', () => {
	assert.deepEqual(
		getMediaAccessErrorDetails(
			'microphone',
			{ name: 'NotAllowedError', message: 'blocked by WebView' },
			{
				origin: 'companion://app',
				isSecureContext: true,
				hasMediaDevices: true,
				hasGetUserMedia: true
			}
		),
		{
				name: 'NotAllowedError',
				message: 'blocked by WebView',
				origin: 'companion://app',
				isSecureContext: true,
				hasMediaDevices: true,
				hasGetUserMedia: true,
				category: 'permission-denied',
				userMessage: 'Microphone access was denied.'
			}
	);
});
