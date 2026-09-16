// Prompt output contract: every action list, direction list, anchor id,
// and numeric range the model sees in its JSON schema is generated from
// the same runtime constants the renderer and parsers enforce. Editing a
// constant (a new anchor, a wider zoom range, a new AI action) updates the
// prompt automatically; nothing here may hardcode values that live
// elsewhere. Pure and node-safe.
import {
	aiAllowedActions,
	isLocomotionActionName,
	LOCOMOTION_ACTIONS,
	LOCOMOTION_DIRECTIONS,
	TURN_DIRECTIONS,
	WALK_DURATION_MAX_MS,
	WALK_DURATION_MIN_MS
} from '../engine/avatar-actions.ts';
import { DEFAULT_SCENE_ANCHORS, type SceneAnchor } from '../engine/scene-anchors.ts';
import { TOUCH_ZONES } from '../engine/photo-reactions.ts';
import {
	EMOTIONAL_EXPRESSIONS,
	EXPRESSION_CUE_DURATION_MAX_MS,
	EXPRESSION_CUE_DURATION_MIN_MS
} from '../engine/facial-expressions.ts';
import { CAMERA_LIMITS } from '../stores/display-types.ts';

// `a, b, or c` prose join for catalog lines.
export function oxfordJoin(values: readonly string[]): string {
	if (values.length === 0) return '';
	if (values.length === 1) return values[0];
	if (values.length === 2) return `${values[0]} or ${values[1]}`;
	return `${values.slice(0, -1).join(', ')}, or ${values[values.length - 1]}`;
}

// Schema alternation: `a|b|c`.
function alt(values: readonly string[]): string {
	return values.join('|');
}

export function animationActionList(): string {
	return alt(
		aiAllowedActions()
			.filter((def) => !isLocomotionActionName(def.id))
			.map((def) => def.id)
	);
}

export function locomotionActionList(): string {
	return alt(LOCOMOTION_ACTIONS);
}

export function locomotionDirectionList(): string {
	return alt(LOCOMOTION_DIRECTIONS);
}

export function turnDirectionList(): string {
	return alt(TURN_DIRECTIONS);
}

export function sceneAnchorList(anchors: readonly SceneAnchor[] = DEFAULT_SCENE_ANCHORS): string {
	return alt(anchors.map((anchor) => anchor.id));
}

export function reactionZoneList(): string {
	return alt(TOUCH_ZONES);
}

export function expressionList(): string {
	return alt(EMOTIONAL_EXPRESSIONS);
}

// Full `gesture_cue` schema line for the JSON output contract.
export function buildAvatarOutputContract(
	anchors: readonly SceneAnchor[] = DEFAULT_SCENE_ANCHORS
): string {
	return `"gesture_cue": null | { "type": "animation", "action": "${animationActionList()}", "direction": "${turnDirectionList()} (turn only)", "anchor_id": "${sceneAnchorList(anchors)} (goto/sit only)" } | { "type": "locomotion", "action": "${locomotionActionList()}", "direction": "${locomotionDirectionList()}", "duration_ms": ${WALK_DURATION_MIN_MS} to ${WALK_DURATION_MAX_MS} } | { "type": "reaction", "zone": "${reactionZoneList()}" }`;
}

// Full `expression_cue` schema line.
export function buildExpressionOutputContract(): string {
	return `"expression_cue": null | { "expression": "${expressionList()}", "intensity": 0 to 1, "duration_ms": ${EXPRESSION_CUE_DURATION_MIN_MS} to ${EXPRESSION_CUE_DURATION_MAX_MS} }`;
}

// Full `camera_cue` schema line, bounded by the UI slider limits.
export function buildCameraOutputContract(): string {
	const { zoom, height, fov } = CAMERA_LIMITS;
	return `"camera_cue": null | { "follow": true|false, "zoom": ${zoom.min} to ${zoom.max}, "height": ${height.min} to ${height.max}, "fov": ${fov.min} to ${fov.max}, "reframe": true (re-center on her now) }`;
}

// Parameterized catalog lines for the avatar capability catalog. Lead text
// comes from the registry descriptions; every value list is generated.
export function buildAvatarCatalogExtras(
	anchors: readonly SceneAnchor[] = DEFAULT_SCENE_ANCHORS
): string[] {
	const defs = aiAllowedActions();
	const walk = defs.find((def) => def.id === 'walk');
	const run = defs.find((def) => def.id === 'run');
	const turn = defs.find((def) => def.id === 'turn');
	const goto = defs.find((def) => def.id === 'goto');
	const lines: string[] = [];
	if (walk) lines.push(`- walk: ${walk.description} (direction ${oxfordJoin(LOCOMOTION_DIRECTIONS)})`);
	if (run) lines.push(`- run: ${run.description} (same directions, faster)`);
	if (turn) lines.push(`- turn: ${turn.description} (direction ${oxfordJoin(TURN_DIRECTIONS)})`);
	if (goto)
		lines.push(
			`- goto: ${goto.description} (anchor_id ${oxfordJoin(anchors.map((anchor) => anchor.id))})`
		);
	lines.push(`- reaction: flinch toward being touched (zone ${oxfordJoin(TOUCH_ZONES)})`);
	return lines;
}

// Shared cue/staging rules: identical in every mode so the model hears one
// voice about gestures, the camera, and completion honesty.
export function buildGestureRules(): string {
	return 'BODY GESTURE RULES: default to gesture_cue null. For ordinary conversation, acknowledgements, questions, thinking, waiting, tool use, and neutral replies, gesture_cue MUST be null. Never gesture just because you are speaking, thinking, waiting, or using a tool; never as filler; never repeat the same gesture in adjacent turns; never because mood changed. Nod only for meaningful agreement, wave mainly for greeting or goodbye, and jump, dance, walk, run, turn, goto, sit, stand, or any position move only when the user explicitly asks or the action itself is the interaction. If uncertain whether a gesture adds value, output null.';
}

export function buildCameraRules(): string {
	return 'CAMERA RULES: default to camera_cue null. Touch the camera only when the user explicitly asks about the view, framing, zoom, or following ("zoom in", "follow me", "stop following", "center the camera"). Set only the keys they asked about; never reframe or move the camera unprompted, and never fight framing they set themselves.';
}

export function buildCompletionRules(): string {
	return 'COMPLETION HONESTY: never claim an action, task, message send, avatar routine, or external effect completed unless the runtime result confirms it. Your own text is never evidence. Do not say done, finished, moved, sent, or completed unless the corresponding receipt says completed.';
}
