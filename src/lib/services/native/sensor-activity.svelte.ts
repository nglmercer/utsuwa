import { browser } from '$app/environment';
import { getBridge } from './bridge';
import { HOST_EVENTS, subscribeHostEvents } from './host-events';
import {
	EMPTY_SENSOR_ACTIVITY,
	parseSensorActivity,
	type SensorActivityState
} from './sensor-activity';

let camera = $state<SensorActivityState>({ ...EMPTY_SENSOR_ACTIVITY });
let microphone = $state<SensorActivityState>({ ...EMPTY_SENSOR_ACTIVITY });
let attached = false;

export function attachSensorActivityListener() {
	if (!browser || attached) return;
	attached = true;
	subscribeHostEvents(
		(event, data) => {
			if (event !== HOST_EVENTS.CAMERA_ACTIVITY_CHANGED && event !== HOST_EVENTS.MICROPHONE_ACTIVITY_CHANGED) {
				return null;
			}
			const next = parseSensorActivity(data);
			return next ? { event, next } : null;
		},
		({ event, next }) => {
			if (event === HOST_EVENTS.CAMERA_ACTIVITY_CHANGED) camera = next;
			if (event === HOST_EVENTS.MICROPHONE_ACTIVITY_CHANGED) microphone = next;
		}
	);
	void refreshSensorActivity();
}

export function cameraActivityState(): SensorActivityState {
	return camera;
}

export function microphoneActivityState(): SensorActivityState {
	return microphone;
}

/** Hydrate from authoritative host state in case a transition preceded mount. */
export async function refreshSensorActivity(): Promise<void> {
	const bridge = getBridge();
	if (!bridge) return;
	await Promise.all([
		bridge
			.invoke('camera.activity.status', {})
			.then((value) => {
				const next = parseSensorActivity(value);
				if (next) camera = next;
			})
			.catch(() => undefined),
		bridge
			.invoke('microphone.activity.status', {})
			.then((value) => {
				const next = parseSensorActivity(value);
				if (next) microphone = next;
			})
			.catch(() => undefined)
	]);
}
