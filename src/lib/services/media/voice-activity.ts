/** Default one-utterance voice activity timings. */
export const DEFAULT_INITIAL_SILENCE_MS = 5_000;
export const DEFAULT_SPEECH_END_SILENCE_MS = 1_000;
export const DEFAULT_MAX_RECORDING_MS = 45_000;
export const DEFAULT_SPEECH_CONFIRMATION_MS = 100;

export type VoiceActivityEvent =
	| 'speech-start'
	| 'speech-end'
	| 'initial-silence'
	| 'maximum-duration';

export interface VoiceActivityDiagnostics {
	currentRms: number;
	peakRms: number;
	noiseFloor: number;
	speechThreshold: number;
	speechCandidateActive: boolean;
	speechDetected: boolean;
	silenceDurationMs: number;
	vadEvent?: VoiceActivityEvent;
}

export interface VoiceActivityConfig {
	initialSilenceMs?: number;
	speechEndSilenceMs?: number;
	maxRecordingMs?: number;
	speechConfirmationMs?: number;
	minSpeechRms?: number;
	noiseMultiplier?: number;
	noiseSmoothing?: number;
}

interface ResolvedVoiceActivityConfig {
	initialSilenceMs: number;
	speechEndSilenceMs: number;
	maxRecordingMs: number;
	speechConfirmationMs: number;
	minSpeechRms: number;
	noiseMultiplier: number;
	noiseSmoothing: number;
}

const DEFAULT_CONFIG: ResolvedVoiceActivityConfig = {
	initialSilenceMs: DEFAULT_INITIAL_SILENCE_MS,
	speechEndSilenceMs: DEFAULT_SPEECH_END_SILENCE_MS,
	maxRecordingMs: DEFAULT_MAX_RECORDING_MS,
	speechConfirmationMs: DEFAULT_SPEECH_CONFIRMATION_MS,
	// Low-gain laptop/WebView microphones often produce speech RMS values in
	// the 0.015–0.04 range. Keep the floor low and let the noise multiplier
	// reject a quiet room instead of requiring a loud microphone.
	minSpeechRms: 0.015,
	noiseMultiplier: 2.2,
	noiseSmoothing: 0.05
};

/** Convert Web Audio byte time-domain samples into normalized RMS energy. */
export function calculateRms(samples: Uint8Array): number {
	if (samples.length === 0) return 0;

	let sumSquares = 0;
	for (const sample of samples) {
		const centered = (sample - 128) / 128;
		sumSquares += centered * centered;
	}
	return Math.sqrt(sumSquares / samples.length);
}

/**
 * Small dependency-free VAD state machine for one spoken utterance.
 *
 * The noise floor adapts only while speech has not been confirmed. A loud
 * frame must persist briefly before it counts as speech, and only confirmed
 * speech can start the end-of-utterance silence window.
 */
export class VoiceActivityDetector {
	private readonly config: ResolvedVoiceActivityConfig;
	private startedAt: number | null = null;
	private noiseFloor = 0.005;
	private speechCandidateAt: number | null = null;
	private silenceStartedAt: number | null = null;
	private speechDetected = false;
	private terminal = false;
	private currentRms = 0;
	private peakRms = 0;
	private lastEvent: VoiceActivityEvent | undefined;

	constructor(config: VoiceActivityConfig = {}) {
		this.config = { ...DEFAULT_CONFIG, ...config };
	}

	start(now: number): void {
		this.startedAt = now;
		this.noiseFloor = 0.005;
		this.speechCandidateAt = null;
		this.silenceStartedAt = null;
		this.speechDetected = false;
		this.terminal = false;
		this.currentRms = 0;
		this.peakRms = 0;
		this.lastEvent = undefined;
	}

	reset(): void {
		this.startedAt = null;
		this.noiseFloor = 0.005;
		this.speechCandidateAt = null;
		this.silenceStartedAt = null;
		this.speechDetected = false;
		this.terminal = false;
		this.currentRms = 0;
		this.peakRms = 0;
		this.lastEvent = undefined;
	}

	get hasDetectedSpeech(): boolean {
		return this.speechDetected;
	}

	get currentThreshold(): number {
		return Math.max(this.config.minSpeechRms, this.noiseFloor * this.config.noiseMultiplier);
	}

	get currentRmsValue(): number {
		return this.currentRms;
	}

	get peakRmsValue(): number {
		return this.peakRms;
	}

	get currentNoiseFloor(): number {
		return this.noiseFloor;
	}

	get isSpeechCandidateActive(): boolean {
		return this.speechCandidateAt !== null && !this.speechDetected;
	}

	get lastVadEvent(): VoiceActivityEvent | undefined {
		return this.lastEvent;
	}

	getSilenceDuration(now: number): number {
		if (this.silenceStartedAt === null) return 0;
		return Math.max(0, now - this.silenceStartedAt);
	}

	getDiagnostics(now: number): VoiceActivityDiagnostics {
		return {
			currentRms: this.currentRms,
			peakRms: this.peakRms,
			noiseFloor: this.noiseFloor,
			speechThreshold: this.currentThreshold,
			speechCandidateActive: this.isSpeechCandidateActive,
			speechDetected: this.speechDetected,
			silenceDurationMs: this.getSilenceDuration(now),
			vadEvent: this.lastEvent
		};
	}

	update(rms: number, now: number): VoiceActivityEvent | null {
		if (this.startedAt === null) this.start(now);
		if (this.terminal || this.startedAt === null) return null;

		const elapsed = Math.max(0, now - this.startedAt);
		const level = Number.isFinite(rms) ? Math.max(0, rms) : 0;
		this.currentRms = level;
		this.peakRms = Math.max(this.peakRms, level);
		if (elapsed >= this.config.maxRecordingMs) {
			this.terminal = true;
			return this.emit('maximum-duration');
		}

		const threshold = this.currentThreshold;
		if (level >= threshold) {
			this.silenceStartedAt = null;
			this.speechCandidateAt ??= now;
			if (
				!this.speechDetected &&
				now - this.speechCandidateAt >= this.config.speechConfirmationMs
			) {
				this.speechDetected = true;
				return this.emit('speech-start');
			}
		} else {
			this.speechCandidateAt = null;
			if (!this.speechDetected) {
				const smoothing = this.config.noiseSmoothing;
				this.noiseFloor = this.noiseFloor * (1 - smoothing) + level * smoothing;
			} else {
				this.silenceStartedAt ??= now;
				if (now - this.silenceStartedAt >= this.config.speechEndSilenceMs) {
					this.terminal = true;
					return this.emit('speech-end');
				}
			}
		}

		if (!this.speechDetected && elapsed >= this.config.initialSilenceMs) {
			this.terminal = true;
			return this.emit('initial-silence');
		}
		return null;
	}

	private emit(event: VoiceActivityEvent): VoiceActivityEvent {
		this.lastEvent = event;
		return event;
	}
}
