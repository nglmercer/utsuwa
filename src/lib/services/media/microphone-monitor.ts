import {
	getMediaAccessErrorDetails,
	type MediaAccessErrorDetails
} from './media-errors.ts';
import { calculateRms } from './voice-activity.ts';

export type MicrophoneMonitorState = 'idle' | 'requesting' | 'monitoring' | 'error';

export type MicrophoneMonitorError = MediaAccessErrorDetails;

export interface MicrophoneMonitorCallbacks {
	onLevel?: (level: number) => void;
	onStateChange?: (state: MicrophoneMonitorState) => void;
	onError?: (error: MicrophoneMonitorError) => void;
}

/** Convert microphone RMS into a useful 0–1 display value. */
export function normalizeMicrophoneLevel(rms: number): number {
	if (!Number.isFinite(rms)) return 0;
	return Math.min(1, Math.max(0, Math.round(rms * 6 * 100) / 100));
}

/**
 * Captures a microphone only for diagnostics. It never creates a MediaRecorder
 * and never sends audio to an STT provider.
 */
export class MicrophoneMonitor {
	private readonly callbacks: MicrophoneMonitorCallbacks;
	private state: MicrophoneMonitorState = 'idle';
	private error: MicrophoneMonitorError | null = null;
	private level = 0;
	private stream: MediaStream | null = null;
	private audioContext: AudioContext | null = null;
	private analyser: AnalyserNode | null = null;
	private animationFrameId: number | null = null;
	private sessionId = 0;

	constructor(callbacks: MicrophoneMonitorCallbacks = {}) {
		this.callbacks = callbacks;
	}

	isSupported(): boolean {
		return typeof navigator !== 'undefined' && !!navigator.mediaDevices?.getUserMedia;
	}

	getState(): MicrophoneMonitorState {
		return this.state;
	}

	getError(): MicrophoneMonitorError | null {
		return this.error;
	}

	getLevel(): number {
		return this.level;
	}

	getHasLevelMeter(): boolean {
		return this.analyser !== null;
	}

	async start(): Promise<boolean> {
		if (this.state === 'monitoring') return true;
		if (this.state === 'requesting') return false;

		this.stop();
		const sessionId = ++this.sessionId;
		this.error = null;
		this.setLevel(0);
		this.setState('requesting');

		if (!this.isSupported()) {
			this.fail({ name: 'NotSupportedError', message: 'navigator.mediaDevices.getUserMedia is unavailable' });
			return false;
		}

		let stream: MediaStream;
		try {
			stream = await navigator.mediaDevices.getUserMedia({ audio: true });
		} catch (error) {
			if (sessionId !== this.sessionId) return false;
			this.fail(error);
			return false;
		}

		// A stop can happen while the native/WebView permission prompt is open.
		if (sessionId !== this.sessionId) {
			stream.getTracks().forEach((track) => track.stop());
			return false;
		}

		this.stream = stream;
		this.stream.getTracks().forEach((track) => {
			track.onended = () => {
				if (sessionId !== this.sessionId || this.state !== 'monitoring') return;
				this.fail(
					{ name: 'NotReadableError', message: 'The microphone stopped delivering audio' },
					'Microphone disconnected. Check that the device is still connected.'
				);
			};
		});

		this.setupAudioAnalysis();
		this.setState('monitoring');
		this.startLevelMonitoring(sessionId);
		return true;
	}

	stop(): void {
		this.sessionId++;
		this.stopLevelMonitoring();
		this.releaseMedia();
		this.error = null;
		this.setLevel(0);
		this.setState('idle');
	}

	private setState(state: MicrophoneMonitorState): void {
		this.state = state;
		this.callbacks.onStateChange?.(state);
	}

	private setLevel(level: number): void {
		this.level = level;
		this.callbacks.onLevel?.(level);
	}

	private fail(error: unknown, userMessage?: string): void {
		this.stopLevelMonitoring();
		this.releaseMedia();
		const details = getMediaAccessErrorDetails('microphone', error);
		this.error = userMessage ? { ...details, userMessage } : details;
		this.setLevel(0);
		this.setState('error');
		this.callbacks.onError?.(this.error);
	}

	private setupAudioAnalysis(): void {
		if (!this.stream || typeof AudioContext === 'undefined') return;

		try {
			this.audioContext = new AudioContext();
			const source = this.audioContext.createMediaStreamSource(this.stream);
			this.analyser = this.audioContext.createAnalyser();
			this.analyser.fftSize = 256;
			source.connect(this.analyser);
			void this.audioContext.resume().catch(() => {
				// A suspended context still proves that getUserMedia succeeded.
			});
		} catch {
			void this.audioContext?.close().catch(() => undefined);
			this.audioContext = null;
			this.analyser = null;
		}
	}

	private startLevelMonitoring(sessionId: number): void {
		if (!this.analyser || typeof requestAnimationFrame === 'undefined') return;

		const dataArray = new Uint8Array(this.analyser.fftSize);
		const tick = () => {
			if (!this.analyser || this.state !== 'monitoring' || sessionId !== this.sessionId) return;
			this.analyser.getByteTimeDomainData(dataArray);
			this.setLevel(normalizeMicrophoneLevel(calculateRms(dataArray)));
			this.animationFrameId = requestAnimationFrame(tick);
		};
		this.animationFrameId = requestAnimationFrame(tick);
	}

	private stopLevelMonitoring(): void {
		if (this.animationFrameId !== null && typeof cancelAnimationFrame !== 'undefined') {
			cancelAnimationFrame(this.animationFrameId);
		}
		this.animationFrameId = null;
	}

	private releaseMedia(): void {
		if (this.stream) {
			this.stream.getTracks().forEach((track) => {
				track.onended = null;
				track.stop();
			});
			this.stream = null;
		}
		if (this.audioContext) {
			void this.audioContext.close().catch(() => undefined);
			this.audioContext = null;
		}
		this.analyser = null;
	}
}
