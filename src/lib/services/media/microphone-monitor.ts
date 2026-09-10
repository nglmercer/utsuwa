import {
	getMediaAccessErrorDetails,
	type MediaAccessErrorDetails
} from './media-errors.ts';
import {
	isNativeAudioCaptureAvailable,
	NativeAudioCaptureBackend
} from '../audio/audio-capture.ts';
import { calculateRms } from './voice-activity.ts';

export type MicrophoneMonitorState = 'idle' | 'requesting' | 'monitoring' | 'error';

export type MicrophoneMonitorError = MediaAccessErrorDetails;

export interface MicrophoneMonitorMetrics {
	currentRms: number;
	peakRms: number;
}

export interface MicrophoneMonitorCallbacks {
	onLevel?: (level: number) => void;
	onMetrics?: (metrics: MicrophoneMonitorMetrics) => void;
	onStateChange?: (state: MicrophoneMonitorState) => void;
	onError?: (error: MicrophoneMonitorError) => void;
}

const NATIVE_MONITOR_MAX_DURATION_MS = 24 * 60 * 60 * 1_000;

/** Convert microphone RMS into a useful 0–1 display value. */
export function normalizeMicrophoneLevel(rms: number): number {
	if (!Number.isFinite(rms)) return 0;
	return Math.min(1, Math.max(0, Math.round(rms * 6 * 100) / 100));
}

/**
 * Captures a microphone only for diagnostics. Native hosts use CPAL directly;
 * browsers use getUserMedia and Web Audio. It never sends audio to an STT
 * provider.
 */
export class MicrophoneMonitor {
	private readonly callbacks: MicrophoneMonitorCallbacks;
	private state: MicrophoneMonitorState = 'idle';
	private error: MicrophoneMonitorError | null = null;
	private level = 0;
	private stream: MediaStream | null = null;
	private audioContext: AudioContext | null = null;
	private analyser: AnalyserNode | null = null;
	private nativeBackend: NativeAudioCaptureBackend | null = null;
	private nativeCancelPromise: Promise<void> | null = null;
	private animationFrameId: number | null = null;
	private sessionId = 0;
	private currentRms = 0;
	private peakRms = 0;

	constructor(callbacks: MicrophoneMonitorCallbacks = {}) {
		this.callbacks = callbacks;
	}

	isSupported(): boolean {
		return isNativeAudioCaptureAvailable() ||
			(typeof navigator !== 'undefined' && !!navigator.mediaDevices?.getUserMedia);
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

	getCurrentRms(): number {
		return this.currentRms;
	}

	getPeakRms(): number {
		return this.peakRms;
	}

	getHasLevelMeter(): boolean {
		return this.analyser !== null || this.nativeBackend !== null;
	}

	async start(): Promise<boolean> {
		if (this.state === 'monitoring') return true;
		if (this.state === 'requesting') return false;

		const pendingStop = this.stop();
		const expectedSessionAfterStop = this.sessionId;
		this.error = null;
		this.setLevel(0);
		this.resetMetrics();
		this.setState('requesting');
		await pendingStop;
		if (this.sessionId !== expectedSessionAfterStop) return false;
		const sessionId = ++this.sessionId;

		if (isNativeAudioCaptureAvailable()) {
			return this.startNative(sessionId);
		}

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
		console.debug('[MicrophoneMonitor] microphone-granted', { trackCount: stream.getTracks().length });
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

	async stop(): Promise<void> {
		this.sessionId++;
		this.stopLevelMonitoring();
		const pendingNativeCancel = this.releaseNativeCapture();
		this.releaseMedia();
		this.error = null;
		this.setLevel(0);
		this.resetMetrics();
		this.setState('idle');
		if (pendingNativeCancel) await pendingNativeCancel;
	}

	private setState(state: MicrophoneMonitorState): void {
		this.state = state;
		this.callbacks.onStateChange?.(state);
	}

	private async startNative(sessionId: number): Promise<boolean> {
		const backend = new NativeAudioCaptureBackend();
		this.nativeBackend = backend;
		try {
			await backend.start({
				autoStop: false,
				maxDurationMs: NATIVE_MONITOR_MAX_DURATION_MS,
				retainAudio: false,
				onAudioLevel: (rms, peakRms) => {
					if (sessionId !== this.sessionId || this.state === 'idle') return;
					this.setMetrics(rms, peakRms);
					this.setLevel(normalizeMicrophoneLevel(rms));
				},
				onStopped: (reason) => {
					if (sessionId !== this.sessionId) return;
					if (reason === 'error') {
						this.fail(new Error('Native microphone capture stopped unexpectedly.'));
						return;
					}
					this.releaseNativeCapture();
					this.setLevel(0);
					this.resetMetrics();
					this.setState('idle');
				}
			});
		} catch (error) {
			if (sessionId !== this.sessionId) return false;
			const message = error instanceof Error ? error.message : 'Native microphone capture failed.';
			this.fail(error, `Native microphone error: ${message}`);
			if (this.nativeCancelPromise) await this.nativeCancelPromise;
			return false;
		}

		if (sessionId !== this.sessionId) {
			this.releaseNativeCapture();
			return false;
		}
		console.debug('[MicrophoneMonitor] native-cpal microphone-granted');
		this.setState('monitoring');
		return true;
	}

	private setLevel(level: number): void {
		this.level = level;
		this.callbacks.onLevel?.(level);
	}

	private setMetrics(currentRms: number, peakRms = currentRms): void {
		this.currentRms = currentRms;
		this.peakRms = Math.max(this.peakRms, currentRms, peakRms);
		this.callbacks.onMetrics?.({ currentRms: this.currentRms, peakRms: this.peakRms });
	}

	private resetMetrics(): void {
		this.currentRms = 0;
		this.peakRms = 0;
		this.callbacks.onMetrics?.({ currentRms: 0, peakRms: 0 });
	}

	private fail(error: unknown, userMessage?: string): void {
		this.stopLevelMonitoring();
		this.releaseNativeCapture();
		this.releaseMedia();
		const details = getMediaAccessErrorDetails('microphone', error);
		console.warn('[MicrophoneMonitor] microphone-error', details);
		this.error = userMessage ? { ...details, userMessage } : details;
		this.setLevel(0);
		this.resetMetrics();
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
			const currentRms = calculateRms(dataArray);
			this.setMetrics(currentRms);
			this.setLevel(normalizeMicrophoneLevel(currentRms));
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

	private releaseNativeCapture(): Promise<void> | null {
		const backend = this.nativeBackend;
		this.nativeBackend = null;
		if (!backend) return this.nativeCancelPromise;

		const cancellation = backend.cancel().catch((error) => {
			console.debug('[MicrophoneMonitor] native-cancel-failed', error);
		});
		this.nativeCancelPromise = cancellation;
		void cancellation.finally(() => {
			if (this.nativeCancelPromise === cancellation) this.nativeCancelPromise = null;
		});
		return cancellation;
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
