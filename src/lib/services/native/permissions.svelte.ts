import { browser } from '$app/environment';
import { getBridge } from './bridge';
import { HOST_EVENTS, subscribeHostEvents } from './host-events';
import {
	parsePermissionRequest,
	replyParams,
	type LifetimeChoice,
	type PermissionRequest
} from './permissions';

// Reactive queue of permission requests awaiting a human decision.
// Fed by `permission.requested` host events; answered through the typed
// `permission.approve` / `permission.deny` IPC methods.
let requests = $state<PermissionRequest[]>([]);
let attached = false;

type PermissionPush =
	| { kind: 'dismissed'; id: string }
	| { kind: 'requested'; request: PermissionRequest };

function parsePermissionPush(event: string, data: Record<string, unknown>): PermissionPush | null {
	if (event === HOST_EVENTS.PERMISSION_DISMISSED) {
		const id = data.id;
		return typeof id === 'string' ? { kind: 'dismissed', id } : null;
	}
	if (event !== HOST_EVENTS.PERMISSION_REQUESTED) return null;
	const request = parsePermissionRequest(data);
	return request ? { kind: 'requested', request } : null;
}

export function attachPermissionListener() {
	if (!browser || attached) return;
	attached = true;
	subscribeHostEvents(parsePermissionPush, (push) => {
		if (push.kind === 'dismissed') {
			requests = requests.filter((request) => request.id !== push.id);
			return;
		}
		if (!requests.some((r) => r.id === push.request.id)) {
			requests.push(push.request);
		}
	});
	// Sync queued requests: push events fired before mount (e.g. boot-time
	// approvals) would otherwise be missed. Absent bridge = plain browser.
	void getBridge()
		?.invoke('permission.list', {})
		.then((result) => {
			if (!Array.isArray(result)) return;
			for (const item of result) {
				const request = parsePermissionRequest(item);
				if (request && !requests.some((r) => r.id === request.id)) {
					requests.push(request);
				}
			}
		})
		.catch(() => {
			// Host without the approval queue (or unreachable): push events
			// still work if they arrive later.
		});
}

export function permissionRequests(): PermissionRequest[] {
	return requests;
}

/** Answer the head request. Resolves true when the host accepted. */
export async function respondToPermission(
	id: string,
	choice: LifetimeChoice
): Promise<boolean> {
	const bridge = getBridge();
	if (!bridge) return false;
	const { method, params } = replyParams(id, choice);
	try {
		await bridge.invoke(method, params);
	} catch {
		return false;
	}
	requests = requests.filter((r) => r.id !== id);
	return true;
}
