// Third-person camera follow with a deadzone. The avatar roams freely
// inside FOLLOW_DEADZONE_M of the frame center (so walks read as motion —
// across the viewport AND closer/farther), and the camera only chases the
// excess beyond the deadzone, preserving the user's orbit offset. Pure and
// node-safe; Scene.svelte applies the returned push to both controls.target
// and the camera position each frame.

// Meters she can stray from the frame center before the camera moves.
export const FOLLOW_DEADZONE_M = 0.35;
// Chase rate once past the deadzone, per second.
export const FOLLOW_GAIN_PER_S = 3;

// Per-frame camera push toward the avatar's plane offset (dx, dz) from the
// orbit target. Zero inside the deadzone; otherwise a gain-limited fraction
// of the excess, so the camera eases her back toward the deadzone rim
// without ever snapping. NaN-safe: garbage in holds the camera still.
export function followPushDelta(
	dx: number,
	dz: number,
	deltaSeconds: number,
	deadzoneM: number = FOLLOW_DEADZONE_M,
	gainPerS: number = FOLLOW_GAIN_PER_S
): { x: number; z: number } {
	const still = { x: 0, z: 0 };
	if (!Number.isFinite(dx) || !Number.isFinite(dz) || !Number.isFinite(deltaSeconds)) return still;
	if (!(deltaSeconds > 0)) return still;
	const deadzone = Number.isFinite(deadzoneM) && deadzoneM > 0 ? deadzoneM : FOLLOW_DEADZONE_M;
	const gain = Number.isFinite(gainPerS) && gainPerS > 0 ? gainPerS : FOLLOW_GAIN_PER_S;
	const offset = Math.hypot(dx, dz);
	if (!(offset > deadzone)) return still;
	const k = Math.min(1, deltaSeconds * gain);
	const push = ((offset - deadzone) / offset) * k;
	return { x: dx * push, z: dz * push };
}
