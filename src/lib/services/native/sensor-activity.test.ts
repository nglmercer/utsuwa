import assert from 'node:assert/strict';
import test from 'node:test';
import {
	EMPTY_SENSOR_ACTIVITY,
	parseSensorActivity,
	sensorIndicatorItems
} from './sensor-activity.ts';

test('sensor activity parser keeps authoritative active/session state', () => {
	assert.deepEqual(
		parseSensorActivity({
			active: true,
			session_count: 2,
			device: 'Built-in Microphone',
			started_at: 123
		}),
		{
			active: true,
			session_count: 2,
			device: 'Built-in Microphone',
			started_at: 123
		}
	);
});

test('invalid sensor activity payloads cannot render an active indicator', () => {
	assert.equal(parseSensorActivity(null), null);
	assert.deepEqual(parseSensorActivity({ active: false }), {
		active: false,
		session_count: 0,
		device: null,
		started_at: null
	});
});

test('sensor indicator rows reflect active host state, including both sensors and counts', () => {
	assert.deepEqual(sensorIndicatorItems(EMPTY_SENSOR_ACTIVITY, EMPTY_SENSOR_ACTIVITY), []);
	assert.deepEqual(
		sensorIndicatorItems(
			{ ...EMPTY_SENSOR_ACTIVITY, active: true, session_count: 2, device: 'front' },
			{ ...EMPTY_SENSOR_ACTIVITY, active: true, session_count: 1, device: 'mic' }
		),
		[
			{ kind: 'camera', label: 'Camera', session_count: 2, device: 'front' },
			{ kind: 'microphone', label: 'Microphone', session_count: 1, device: 'mic' }
		]
	);
});
