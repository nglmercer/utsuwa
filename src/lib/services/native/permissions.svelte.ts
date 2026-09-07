import { browser } from '$app/environment';
import { getBridge, HOST_EVENT, isHostEvent } from './bridge';
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

function onHostEvent(e: Event) {
	if (!isHostEvent(e)) return;
	const detail = (e as CustomEvent).detail;
	if (!detail || detail.event !== 'permission.requested') return;
	const request = parsePermissionRequest(detail.data);
	if (request && !requests.some((r) => r.id === request.id)) {
		requests.push(request);
	}
}

export function attachPermissionListener() {
	if (!browser || attached) return;
	attached = true;
	window.addEventListener(HOST_EVENT, onHostEvent);
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
