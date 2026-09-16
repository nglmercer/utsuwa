// Scene anchors: named places in the room the avatar can go to or sit on.
// Pure and node-safe (no Svelte, no Three.js) so the command parser,
// response parser, prompt builder, renderer, and tests share one registry.
//
// Coordinates are meters on the avatar plane (X right, Z toward the viewer
// at yaw 0), inside the walk radius. Adding a scenario place is a data
// change: append an entry. Future multi-room scenarios can swap the active
// list; the resolver and prompt layers already take the list as input.
export interface SceneAnchor {
	id: string;
	label: string;
	x: number;
	z: number;
	// Yaw to adopt when arriving (radians, 0 faces the default camera).
	// Absent = keep the arrival heading; face_camera can fix it after.
	yaw?: number;
	sittable?: boolean;
	// Hip height while seated here, in meters. Absent = default chair height.
	seatHeight?: number;
}

// Default room: center stage plus two sittable spots, all inside the 2m
// walk radius. Labels are what users say ("go to the chair").
export const DEFAULT_SCENE_ANCHORS: readonly SceneAnchor[] = [
	{ id: 'center', label: 'center', x: 0, z: 0, yaw: 0 },
	{ id: 'chair', label: 'chair', x: 0.9, z: 0.35, yaw: 0, sittable: true, seatHeight: 0.45 },
	{ id: 'cushion', label: 'cushion', x: -0.9, z: -0.3, yaw: 0, sittable: true, seatHeight: 0.28 }
];

export function listSceneAnchors(): SceneAnchor[] {
	return DEFAULT_SCENE_ANCHORS.map((anchor) => ({ ...anchor }));
}

// Resolve a user/model reference to an anchor: id or label, case and
// whitespace insensitive ("Chair", " the cushion "). Unknown refs are null
// (never a guessed coordinate): callers degrade to sit-in-place or ask.
export function resolveSceneAnchor(
	ref: unknown,
	anchors: readonly SceneAnchor[] = DEFAULT_SCENE_ANCHORS
): SceneAnchor | null {
	if (typeof ref !== 'string') return null;
	const needle = ref.trim().toLowerCase().replace(/^(the|a)\s+/, '');
	if (!needle) return null;
	for (const anchor of anchors) {
		if (anchor.id.toLowerCase() === needle || anchor.label.toLowerCase() === needle) {
			return anchor;
		}
	}
	return null;
}

export function sittableAnchors(
	anchors: readonly SceneAnchor[] = DEFAULT_SCENE_ANCHORS
): SceneAnchor[] {
	return anchors.filter((anchor) => anchor.sittable === true).map((anchor) => ({ ...anchor }));
}
