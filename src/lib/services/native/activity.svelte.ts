import { browser } from '$app/environment';
import { getBridge } from './bridge';
import { HOST_EVENTS, subscribeHostEvents } from './host-events';
import {
	parseActivityList,
	type ActivityRecord
} from './activity';

// Reactive mirror of the host audit trail for the Activity panel.
// Fed on demand through `activity.list` plus live `agent.turn_done`
// refreshes; the host keeps records redacted and bounded.
let records = $state<ActivityRecord[]>([]);
let attached = false;

export function attachActivityListener() {
	if (!browser || attached) return;
	attached = true;
	subscribeHostEvents(
		(event) => (event === HOST_EVENTS.AGENT_DONE ? true : null),
		() => {
			// A finished turn executed tools: refresh the trail. Best-effort;
			// the panel also refreshes on mount and on demand.
			void refreshActivity();
		}
	);
	void refreshActivity();
}

export function activityRecords(): ActivityRecord[] {
	return records;
}

/** Pull the latest trail from the host. No-op without a bridge. */
export async function refreshActivity(limit = 50): Promise<void> {
	const bridge = getBridge();
	if (!bridge) return;
	try {
		const result = await bridge.invoke('activity.list', { limit });
		records = parseActivityList(result);
	} catch {
		// Host unreachable or method missing: keep the last snapshot.
	}
}
