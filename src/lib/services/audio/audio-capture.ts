import {
	getBridge,
	HOST_EVENT,
	type HostEventDetail
} from '../native/bridge.ts';
import { isNativeRuntimeAvailable } from '../platform/platform.ts';
import {
	DEFAULT_INITIAL_SILENCE_MS,
	DEFAULT_MAX_RECORDING_MS,
	DEFAULT_SPEECH_END_SILENCE_MS,
	VoiceActivityDetector,
	type VoiceActivityEvent
} from '../media/voice-activity.ts';

export type AudioCaptureBackendName = 'native-cpal' | 'web-media-recorder';
export type AudioCaptureStopReason =
	| 'manual'
	| 'silence-detected'
	| 'maximum-duration'
	| 'microphone-ended'
	| 'error';

export interface AudioCaptureInfo {
	captureId?: string;
	backend: AudioCaptureBackendName;
	device?: string;
	sampleRate?: number;
	channels?: number;
}

export interface AudioCaptureDiagnostics {
	backend: AudioCaptureBackendName;
	device?: string;
	sampleRate?: number;
	channels?: number;
	analyserAvailable?: boolean;
	currentRms: number;
	peakRms: number;
	noiseFloor?: number;
	speechThreshold?: number;
	speechCandidateActive: boolean;
	speechDetected: boolean;
	silenceDurationMs: number;
	vadEvent?: VoiceActivityEvent;
	chunkCount: number;
	recordedBytes: number;
	wavBytes?: number;
	durationMs: number;
	mimeType?: string;
	droppedChunks?: number;
}

export interface CaptureOptions {
	autoStop?: boolean;
	silenceDurationMs?: number;
	maxDurationMs?: number;
	sampleRate?: number;
	onAudioLevel?: (rms: number, peakRms: number) => void;
	onDiagnostics?: (diagnostics: AudioCaptureDiagnostics) => void;
	onStopped?: (reason: AudioCaptureStopReason) => void;
}

export interface AudioCaptureBackend {
	start(options?: CaptureOptions): Promise<void>;
	stop(): Promise<Blob>;
	cancel(): Promise<void>;
	isSupported(): boolean;
	getInfo(): AudioCaptureInfo | null;
	getDiagnostics(): AudioCaptureDiagnostics;
	readonly name: AudioCaptureBackendName;
}

function asRecord(value: unknown): Record<string, unknown> | null {
	return typeof value === 'object' && value !== null && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: null;
}

function asNumber(value: unknown): number | undefined {
	return typeof value === 'number' && Number.isFinite(value) ? value : undefined;
}

function asString(value: unknown): string | undefined {
	return typeof value === 'string' && value.length > 0 ? value : undefined;
}

function stopReason(value: unknown): AudioCaptureStopReason {
	switch (value) {
		case 'silence_detected':
			return 'silence-detected';
		case 'maximum_duration':
			return 'maximum-duration';
		case 'microphone_ended':
			return 'microphone-ended';
		case 'error':
			return 'error';
		default:
			return 'manual';
	}
}

function emptyDiagnostics(backend: AudioCaptureBackendName, mimeType?: string): AudioCaptureDiagnostics {
	return {
		backend,
		currentRms: 0,
		peakRms: 0,
		speechCandidateActive: false,
		speechDetected: false,
		silenceDurationMs: 0,
		chunkCount: 0,
		recordedBytes: 0,
		durationMs: 0,
		...(mimeType ? { mimeType } : {})
	};
}

function nativeHostEvent(event: Event): { data: Record<string, unknown>; event: Record<string, unknown> } | null {
	const custom = event as CustomEvent<HostEventDetail>;
	const detail = custom.detail;
	if (!detail || detail.event !== 'audio.capture') return null;
	const data = asRecord(detail.data);
	const captureEvent = asRecord(data?.event);
	if (!data || !captureEvent) return null;
	return { data, event: captureEvent };
}

/** CPAL-backed desktop capture. Audio bytes stay in the native media registry. */
export class NativeAudioCaptureBackend implements AudioCaptureBackend {
	readonly name = 'native-cpal' as const;

	private options: CaptureOptions = {};
	private captureId: string | null = null;
	private info: AudioCaptureInfo | null = null;
	private diagnostics = emptyDiagnostics('native-cpal', 'audio/wav');
	private lastStopReason: AudioCaptureStopReason = 'manual';
	private stoppedNotified = false;
	private eventListener: ((event: Event) => void) | null = null;

	isSupported(): boolean {
		return isNativeRuntimeAvailable() && getBridge() !== null;
	}

	getInfo(): AudioCaptureInfo | null {
		return this.info;
	}

	getDiagnostics(): AudioCaptureDiagnostics {
		return this.diagnostics;
	}

	async start(options: CaptureOptions = {}): Promise<void> {
		if (!this.isSupported()) throw new Error('Native audio capture is unavailable.');
		if (this.captureId) throw new Error('Native audio capture is already running.');

		const bridge = getBridge();
		if (!bridge) throw new Error('Native host runtime was expected but the Utsuwa IPC bridge is unavailable.');
		this.options = options;
		this.diagnostics = emptyDiagnostics('native-cpal', 'audio/wav');
		this.lastStopReason = 'manual';
		this.stoppedNotified = false;
		this.eventListener = (event) => this.handleHostEvent(event);
		window.addEventListener(HOST_EVENT, this.eventListener);

		try {
			const result = asRecord(
				await bridge.invoke('audio_capture.start', {
					config: {
						auto_stop: options.autoStop !== false,
						silence_duration_ms: options.silenceDurationMs ?? DEFAULT_SPEECH_END_SILENCE_MS,
						max_duration_ms: options.maxDurationMs ?? DEFAULT_MAX_RECORDING_MS,
						sample_rate: options.sampleRate ?? null
					}
				})
			);
			const captureId = asString(result?.capture_id);
			if (!captureId) throw new Error('Native audio capture did not return a capture id.');
			this.captureId = captureId;
			this.info = {
				captureId,
				backend: 'native-cpal',
				device: asString(result?.device),
				sampleRate: asNumber(result?.sample_rate),
				channels: asNumber(result?.channels)
			};
			this.diagnostics = {
				...this.diagnostics,
				device: this.info.device,
				sampleRate: this.info.sampleRate,
				channels: this.info.channels
			};
			this.publishDiagnostics();
		} catch (error) {
			// The host can have opened the device before a malformed response or
			// bridge failure reaches the page. Cancel that session if its id was
			// observed in an early host event.
			if (this.captureId) {
				await bridge.invoke('audio_capture.cancel', {}).catch(() => undefined);
			}
			this.detachEventListener();
			this.captureId = null;
			this.info = null;
			throw error;
		}
	}

	async stop(): Promise<Blob> {
		const captureId = this.captureId;
		if (!captureId) throw new Error('Native audio capture is not running.');
		const bridge = getBridge();
		if (!bridge) throw new Error('Native host runtime was expected but the Utsuwa IPC bridge is unavailable.');

		try {
			const result = asRecord(await bridge.invoke('audio_capture.stop', {}));
			const mediaUrl = asString(result?.media_url) ?? `companion://media/${captureId}`;
			const response = await fetch(mediaUrl);
			if (!response.ok) throw new Error(`Native audio media request failed (${response.status}).`);
			const fetched = await response.blob();
			const mimeType = asString(result?.mime_type) ?? fetched.type ?? 'audio/wav';
			this.updateFinalDiagnostics(result);
			return fetched.type === mimeType ? fetched : new Blob([fetched], { type: mimeType });
		} finally {
			this.detachEventListener();
			this.captureId = null;
			this.info = null;
		}
	}

	async cancel(): Promise<void> {
		const bridge = getBridge();
		if (bridge && this.captureId) {
			try {
				await bridge.invoke('audio_capture.cancel', {});
			} finally {
				this.detachEventListener();
				this.captureId = null;
				this.info = null;
			}
		} else {
			this.detachEventListener();
			this.captureId = null;
			this.info = null;
		}
	}

	private handleHostEvent(event: Event): void {
		const hostEvent = nativeHostEvent(event);
		if (!hostEvent) return;
		const eventCaptureId = asString(hostEvent.data.capture_id);
		if (this.captureId && eventCaptureId && this.captureId !== eventCaptureId) return;
		if (!this.captureId && eventCaptureId) this.captureId = eventCaptureId;

		switch (hostEvent.event.type) {
			case 'audio_level': {
				const rms = asNumber(hostEvent.event.rms) ?? 0;
				const peakRms = asNumber(hostEvent.event.peak_rms) ?? rms;
				this.diagnostics = {
					...this.diagnostics,
					currentRms: rms,
					peakRms: Math.max(this.diagnostics.peakRms, peakRms)
				};
				this.options.onAudioLevel?.(rms, this.diagnostics.peakRms);
				this.publishDiagnostics();
				break;
			}
			case 'speech_started':
				this.diagnostics = { ...this.diagnostics, speechDetected: true, vadEvent: 'speech-start' };
				this.publishDiagnostics();
				break;
			case 'speech_ended':
				this.diagnostics = { ...this.diagnostics, vadEvent: 'speech-end' };
				this.publishDiagnostics();
				break;
			case 'stopped': {
				this.lastStopReason = stopReason(asRecord(hostEvent.event.reason)?.type ?? hostEvent.event.reason);
				if (this.stoppedNotified) break;
				this.stoppedNotified = true;
				this.options.onStopped?.(this.lastStopReason);
				break;
			}
		}
	}

	private updateFinalDiagnostics(result: Record<string, unknown> | null): void {
		const stats = asRecord(result?.stats);
		const wavBytes = asNumber(result?.wav_bytes) ?? 0;
		this.diagnostics = {
			...this.diagnostics,
			durationMs: asNumber(result?.duration_ms) ?? asNumber(stats?.duration_ms) ?? 0,
			recordedBytes: wavBytes,
			wavBytes,
			chunkCount: asNumber(stats?.chunk_count) ?? 0,
			droppedChunks: asNumber(stats?.dropped_chunks) ?? 0,
			currentRms: asNumber(stats?.current_rms) ?? this.diagnostics.currentRms,
			peakRms: asNumber(stats?.peak_rms) ?? this.diagnostics.peakRms,
			noiseFloor: asNumber(stats?.noise_floor),
			speechThreshold: asNumber(stats?.speech_threshold),
			speechCandidateActive: stats?.speech_candidate_active === true,
			speechDetected: stats?.speech_detected === true,
			silenceDurationMs: asNumber(stats?.silence_duration_ms) ?? this.diagnostics.silenceDurationMs,
			mimeType: asString(result?.mime_type) ?? 'audio/wav'
		};
		if (this.lastStopReason === 'silence-detected') {
			this.diagnostics.vadEvent = 'speech-end';
		} else if (this.lastStopReason === 'maximum-duration') {
			this.diagnostics.vadEvent = 'maximum-duration';
		}
		this.publishDiagnostics();
	}

	private publishDiagnostics(): void {
		this.options.onDiagnostics?.(this.diagnostics);
	}

	private detachEventListener(): void {
		if (this.eventListener && typeof window !== 'undefined') {
			window.removeEventListener(HOST_EVENT, this.eventListener);
		}
		this.eventListener = null;
	}
}

/** Browser/WebView fallback. Desktop hosts choose NativeAudioCaptureBackend. */
export class WebMediaRecorderBackend implements AudioCaptureBackend {
	readonly name = 'web-media-recorder' as const;

	private options: CaptureOptions = {};
	private recorder: MediaRecorder | null = null;
	private stream: MediaStream | null = null;
	private chunks: Blob[] = [];
	private analyser: AnalyserNode | null = null;
	private audioContext: AudioContext | null = null;
	private animationFrameId: number | null = null;
	private detector: VoiceActivityDetector | null = null;
	private stopPromise: Promise<Blob> | null = null;
	private resolveStop: ((blob: Blob) => void) | null = null;
	private rejectStop: ((error: unknown) => void) | null = null;
	private stopReason: AudioCaptureStopReason = 'manual';
	private maxTimer: ReturnType<typeof setTimeout> | null = null;
	private recordingStartedAt: number | null = null;
	private info: AudioCaptureInfo | null = null;
	private diagnostics = emptyDiagnostics('web-media-recorder');

	isSupported(): boolean {
		return typeof navigator !== 'undefined' && !!navigator.mediaDevices?.getUserMedia && typeof MediaRecorder !== 'undefined';
	}

	getInfo(): AudioCaptureInfo | null {
		return this.info;
	}

	getDiagnostics(): AudioCaptureDiagnostics {
		return this.diagnostics;
	}

	async start(options: CaptureOptions = {}): Promise<void> {
		if (!this.isSupported()) throw new Error('MediaRecorder microphone capture is unavailable.');
		if (this.recorder) throw new Error('MediaRecorder capture is already running.');
		this.options = options;
		this.stopReason = 'manual';
		this.chunks = [];
		this.diagnostics = emptyDiagnostics('web-media-recorder');
		try {
			this.stream = await navigator.mediaDevices.getUserMedia({ audio: true });
			const track = this.stream.getAudioTracks?.()[0] ?? this.stream.getTracks()[0];
			this.info = {
				backend: 'web-media-recorder',
				sampleRate: track?.getSettings?.().sampleRate,
				channels: track?.getSettings?.().channelCount
			};
			if (track) track.onended = () => this.requestStop('microphone-ended');
			this.setupAnalyser();
			this.diagnostics = {
				...this.diagnostics,
				analyserAvailable: this.analyser !== null
			};
			const mimeType = this.getMimeType();
			this.recorder = mimeType ? new MediaRecorder(this.stream, { mimeType }) : new MediaRecorder(this.stream);
			const recorder = this.recorder;
			recorder.ondataavailable = (event) => {
				if (event.data.size > 0) {
					this.chunks.push(event.data);
					this.diagnostics = {
						...this.diagnostics,
						chunkCount: this.chunks.length,
						recordedBytes: this.chunks.reduce((total, chunk) => total + chunk.size, 0),
						mimeType: event.data.type || recorder.mimeType
					};
				}
			};
			recorder.onerror = () => this.requestStop('error');
			recorder.onstop = () => this.finishStop();
			recorder.start(250);
			this.recordingStartedAt = performance.now();
			this.startVoiceActivity(options);
			this.publishDiagnostics();
		} catch (error) {
			await this.cancel().catch(() => undefined);
			throw error;
		}
	}

	stop(): Promise<Blob> {
		if (this.stopPromise) return this.stopPromise;
		if (!this.recorder) return Promise.reject(new Error('MediaRecorder capture is not running.'));
		const promise = this.createStopPromise();
		this.requestStop('manual');
		return promise;
	}

	async cancel(): Promise<void> {
		this.clearTimers();
		this.stopLevelMonitoring();
		const recorder = this.recorder;
		this.recorder = null;
		if (recorder) {
			recorder.ondataavailable = null;
			recorder.onstop = null;
			recorder.onerror = null;
			if (recorder.state !== 'inactive') {
				try {
					recorder.stop();
				} catch {
					// Cancellation is already tearing down the recorder.
				}
			}
		}
		this.releaseMedia();
		this.rejectStop?.(new Error('Audio capture cancelled.'));
		this.resolveStop = null;
		this.rejectStop = null;
		this.stopPromise = null;
	}

	private requestStop(reason: AudioCaptureStopReason): void {
		if (!this.recorder) return;
		this.createStopPromise();
		this.stopReason = reason;
		this.clearTimers();
		this.stopLevelMonitoring();
		if (this.recorder.state !== 'inactive') {
			try {
				this.recorder.stop();
			} catch (error) {
				this.rejectStop?.(error);
				this.resolveStop = null;
				this.rejectStop = null;
				this.stopPromise = null;
			}
		}
	}

	private createStopPromise(): Promise<Blob> {
		if (this.stopPromise) return this.stopPromise;
		this.stopPromise = new Promise<Blob>((resolve, reject) => {
			this.resolveStop = resolve;
			this.rejectStop = reject;
		});
		return this.stopPromise;
	}

	private finishStop(): void {
		const recorder = this.recorder;
		const mimeType = recorder?.mimeType || this.chunks.find((chunk) => chunk.type)?.type || 'audio/webm';
		const blob = new Blob(this.chunks, { type: mimeType });
		const durationMs = this.recordingStartedAt === null
			? this.diagnostics.durationMs
			: Math.max(0, performance.now() - this.recordingStartedAt);
		const detectorDiagnostics = this.detector?.getDiagnostics(performance.now());
		this.diagnostics = {
			...this.diagnostics,
			recordedBytes: blob.size,
			mimeType,
			durationMs,
			...(detectorDiagnostics ?? {}),
			vadEvent: this.stopReason === 'silence-detected' ? 'speech-end' : this.diagnostics.vadEvent
		};
		this.publishDiagnostics();
		this.recorder = null;
		this.recordingStartedAt = null;
		this.releaseMedia();
		this.resolveStop?.(blob);
		this.options.onStopped?.(this.stopReason);
		this.resolveStop = null;
		this.rejectStop = null;
		this.stopPromise = null;
	}

	private setupAnalyser(): void {
		if (!this.stream || typeof AudioContext === 'undefined') return;
		try {
			this.audioContext = new AudioContext();
			const source = this.audioContext.createMediaStreamSource(this.stream);
			this.analyser = this.audioContext.createAnalyser();
			this.analyser.fftSize = 256;
			source.connect(this.analyser);
			void this.audioContext.resume().catch(() => undefined);
		} catch {
			void this.audioContext?.close().catch(() => undefined);
			this.audioContext = null;
			this.analyser = null;
		}
	}

	private startVoiceActivity(options: CaptureOptions): void {
		this.detector = options.autoStop === false || !this.analyser
			? null
			: new VoiceActivityDetector({
					initialSilenceMs: DEFAULT_INITIAL_SILENCE_MS,
					speechEndSilenceMs: options.silenceDurationMs ?? DEFAULT_SPEECH_END_SILENCE_MS,
					maxRecordingMs: options.maxDurationMs ?? DEFAULT_MAX_RECORDING_MS
				});
		this.detector?.start(performance.now());
		this.maxTimer = setTimeout(() => this.requestStop('maximum-duration'), options.maxDurationMs ?? DEFAULT_MAX_RECORDING_MS);
		this.startLevelMonitoring();
	}

	private startLevelMonitoring(): void {
		if (!this.analyser || typeof requestAnimationFrame === 'undefined') return;
		const data = new Uint8Array(this.analyser.fftSize);
		const tick = () => {
			if (!this.analyser || !this.recorder) return;
			this.analyser.getByteTimeDomainData(data);
			let sum = 0;
			for (const sample of data) {
				const centered = (sample - 128) / 128;
				sum += centered * centered;
			}
			const rms = data.length ? Math.sqrt(sum / data.length) : 0;
			const now = performance.now();
			const detectorEvent = this.detector?.update(rms, now) ?? null;
			const detectorDiagnostics = this.detector?.getDiagnostics(now);
			this.diagnostics = {
				...this.diagnostics,
				currentRms: rms,
				peakRms: Math.max(this.diagnostics.peakRms, rms),
				analyserAvailable: true,
				...(detectorDiagnostics ?? {})
			};
			if (detectorEvent === 'speech-end') this.requestStop('silence-detected');
			else if (detectorEvent === 'initial-silence') this.requestStop('silence-detected');
			else if (detectorEvent === 'maximum-duration') this.requestStop('maximum-duration');
			if (detectorEvent === 'speech-start') this.diagnostics.speechDetected = true;
			this.options.onAudioLevel?.(rms, this.diagnostics.peakRms);
			this.publishDiagnostics();
			if (this.recorder) this.animationFrameId = requestAnimationFrame(tick);
		};
		this.animationFrameId = requestAnimationFrame(tick);
	}

	private stopLevelMonitoring(): void {
		if (this.animationFrameId !== null && typeof cancelAnimationFrame !== 'undefined') cancelAnimationFrame(this.animationFrameId);
		this.animationFrameId = null;
	}

	private clearTimers(): void {
		if (this.maxTimer !== null) clearTimeout(this.maxTimer);
		this.maxTimer = null;
	}

	private releaseMedia(): void {
		this.stopLevelMonitoring();
		this.detector = null;
		this.stream?.getTracks().forEach((track) => {
			track.onended = null;
			track.stop();
		});
		this.stream = null;
		if (this.audioContext) void this.audioContext.close().catch(() => undefined);
		this.audioContext = null;
		this.analyser = null;
	}

	private getMimeType(): string | undefined {
		if (typeof MediaRecorder === 'undefined' || typeof MediaRecorder.isTypeSupported !== 'function') return undefined;
		for (const type of ['audio/webm;codecs=opus', 'audio/webm', 'audio/ogg;codecs=opus', 'audio/mp4']) {
			if (MediaRecorder.isTypeSupported(type)) return type;
		}
		return undefined;
	}

	private publishDiagnostics(): void {
		this.options.onDiagnostics?.(this.diagnostics);
	}
}

export function isNativeAudioCaptureAvailable(): boolean {
	return isNativeRuntimeAvailable() && getBridge() !== null;
}

export function createAudioCaptureBackend(): AudioCaptureBackend {
	return isNativeAudioCaptureAvailable()
		? new NativeAudioCaptureBackend()
		: new WebMediaRecorderBackend();
}
