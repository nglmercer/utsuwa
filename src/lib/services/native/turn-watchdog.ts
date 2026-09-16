// Generic turn watchdog: a hard whole-turn budget plus a no-progress
// budget, both pausable while the turn waits on the user. Framework-free
// so it runs under `node --test`; the agent chat adapter owns the timeout
// semantics (messages, host cancellation) via `onTimeout`.
export interface TurnWatchdogOptions {
	hardTimeoutMs: number;
	progressTimeoutMs: number;
	onTimeout: (message: string) => void;
	hardMessage?: string;
	progressMessage?: string;
}

export interface TurnWatchdog {
	/** Arm both budgets (also re-arms after a pause). */
	start: () => void;
	/** A progress signal arrived: re-arm no-progress, arm hard if idle. */
	progress: () => void;
	/** Suspend both budgets while the turn waits on the user. */
	pause: () => void;
	/** Re-arm both budgets after a pause. */
	resume: () => void;
	/** Disarm everything; the turn settled. */
	stop: () => void;
}

export function createTurnWatchdog(options: TurnWatchdogOptions): TurnWatchdog {
	const hardMessage =
		options.hardMessage ??
		`Agent turn timed out after ${options.hardTimeoutMs / 1000}s without finishing`;
	const progressMessage =
		options.progressMessage ??
		`Agent turn stalled: no progress for ${options.progressTimeoutMs / 1000}s`;
	let hardTimer: ReturnType<typeof setTimeout> | null = null;
	let progressTimer: ReturnType<typeof setTimeout> | null = null;

	const clear = (timer: ReturnType<typeof setTimeout> | null) => {
		if (timer) clearTimeout(timer);
	};
	const armHard = () => {
		clear(hardTimer);
		hardTimer = setTimeout(() => options.onTimeout(hardMessage), options.hardTimeoutMs);
	};
	const armProgress = () => {
		clear(progressTimer);
		progressTimer = setTimeout(() => options.onTimeout(progressMessage), options.progressTimeoutMs);
	};
	return {
		start: () => {
			armHard();
			armProgress();
		},
		progress: () => {
			armProgress();
			if (!hardTimer) armHard();
		},
		pause: () => {
			clear(hardTimer);
			clear(progressTimer);
			hardTimer = null;
			progressTimer = null;
		},
		resume: () => {
			armHard();
			armProgress();
		},
		stop: () => {
			clear(hardTimer);
			clear(progressTimer);
			hardTimer = null;
			progressTimer = null;
		}
	};
}
