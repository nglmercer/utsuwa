export interface SensorActivityState {
	active: boolean;
	session_count: number;
	device: string | null;
	started_at: number | null;
}

export const EMPTY_SENSOR_ACTIVITY: SensorActivityState = {
	active: false,
	session_count: 0,
	device: null,
	started_at: null
};

export interface SensorIndicatorItem {
	kind: 'camera' | 'microphone';
	label: 'Camera' | 'Microphone';
	session_count: number;
	device: string | null;
}

/** Select the persistent indicator rows from authoritative host snapshots. */
export function sensorIndicatorItems(
	camera: SensorActivityState,
	microphone: SensorActivityState
): SensorIndicatorItem[] {
	const items: SensorIndicatorItem[] = [];
	if (camera.active) {
		items.push({ kind: 'camera', label: 'Camera', session_count: camera.session_count, device: camera.device });
	}
	if (microphone.active) {
		items.push({
			kind: 'microphone',
			label: 'Microphone',
			session_count: microphone.session_count,
			device: microphone.device
		});
	}
	return items;
}

/** Parse only the host-owned activity fields used by the persistent chrome. */
export function parseSensorActivity(value: unknown): SensorActivityState | null {
	if (typeof value !== 'object' || value === null) return null;
	const raw = value as Record<string, unknown>;
	return {
		active: raw.active === true,
		session_count:
			typeof raw.session_count === 'number' && Number.isFinite(raw.session_count)
				? Math.max(0, Math.floor(raw.session_count))
				: 0,
		device: typeof raw.device === 'string' ? raw.device : null,
		started_at: typeof raw.started_at === 'number' && Number.isFinite(raw.started_at) ? raw.started_at : null
	};
}
