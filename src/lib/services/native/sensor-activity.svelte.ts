import { browser } from '$app/environment';
import { getBridge, HOST_EVENT, isHostEvent } from './bridge';
import {
	EMPTY_SENSOR_ACTIVITY,
	parseSensorActivity,
	type SensorActivityState
} from './sensor-activity';

let camera = $state<SensorActivityState>({ ...EMPTY_SENSOR_ACTIVITY });
let microphone = $state<SensorActivityState>({ ...EMPTY_SENSOR_ACTIVITY });
let attached = false;

function updateSensor(event: string, data: unknown) {
	const next = parseSensorActivity(data);
	if (!next) return;
	if (event === 'camera.activity.changed') camera = next;
	if (event === 'microphone.activity.changed') microphone = next;
}

function onHostEvent(event: Event) {
	if (!isHostEvent(event)) return;
	const detail = (event as CustomEvent).detail;
	if (!detail) return;
	updateSensor(detail.event, detail.data);
}

export function attachSensorActivityListener() {
	if (!browser || attached) return;
	attached = true;
	window.addEventListener(HOST_EVENT, onHostEvent);
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
