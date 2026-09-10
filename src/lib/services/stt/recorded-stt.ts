import type { SpeechRecognitionCallbacks } from './web-speech.ts';
import {
	getMediaAccessErrorDetails,
	getMediaErrorMessage,
	type MediaAccessErrorDetails
} from '../media/media-errors.ts';
import {
	calculateRms,
	DEFAULT_INITIAL_SILENCE_MS,
	DEFAULT_MAX_RECORDING_MS,
	VoiceActivityDetector,
	type VoiceActivityEvent
} from '../media/voice-activity.ts';

export type RecordingStopReason =
	| 'manual'
	| 'speech-end'
	| 'initial-silence'
	| 'maximum-duration'
	| 'microphone-ended'
	| 'cancelled'
	| 'recorder-error';

export type RecordedSttResultStatus =
	| 'complete'
	| 'no-speech'
	| 'microphone-error'
	| 'recorder-empty'
	| 'empty-recording'
	| 'microphone-ended'
	| 'recorder-error'
	| 'provider-empty'
	| 'provider-error'
	| 'timeout';

export type SttTransportStage =
	| 'upload-start'
	| 'upload-success'
	| 'transcription-start'
	| 'transcription-success';

export type RecordedSttLifecycleState =
	| 'idle'
	| 'requesting-microphone'
	| 'recording'
	| 'transcribing'
	| 'complete'
	| 'error'
	| 'cancelled';

export interface RecordedSttDiagnostics {
	stopReason?: RecordingStopReason;

	vadEnabled: boolean;
	analyserAvailable: boolean;
	speechDetected: boolean;

	currentRms: number;
	peakRms: number;
	noiseFloor?: number;
	speechThreshold?: number;
	speechCandidateActive: boolean;
	silenceDurationMs: number;
	vadEvent?: VoiceActivityEvent;

	mimeType?: string;
	chunkCount: number;
	recordedBytes: number;
	durationMs: number;

	providerStarted: boolean;
	uploadStarted: boolean;
	transcriptionStarted: boolean;
	providerStage?: 'upload' | 'transcription';
}

export interface RecordedSttSessionResult {
	status: RecordedSttResultStatus;
	text?: string;
	error?: string;
	mediaError?: MediaAccessErrorDetails;
	diagnostics: RecordedSttDiagnostics;
}

export interface SttTransportContext {
	signal: AbortSignal;
	filename: string;
	onStage?: (stage: SttTransportStage) => void;
}

/**
 * Provider-independent transport for one-utterance recordings.
 *
 * The recorder owns microphone permissions, MediaRecorder, voice activity,
 * audio levels, and cancellation. Providers only need to turn the finished
 * Blob into text. Manual stop remains available as an explicit fallback.
 */
export interface RecordedSttTransport {
	transcribe(audio: Blob, context: SttTransportContext): Promise<string>;
	timeoutMs?: number;
	timeoutMessage?: string;
	emptyResultMessage?: string;
}

export const DEFAULT_RECORDED_STT_TIMEOUT_MS = 30_000;

export interface RecordedSttStartOptions {
	/** Disable automatic end-of-speech so a diagnostic can isolate the provider. */
	autoStop?: boolean;
}

function currentTime(): number {
	return typeof performance !== 'undefined' ? performance.now() : Date.now();
}

function roundRms(value: number): number {
	return Number(value.toFixed(4));
}

function displayLevelFromRms(rms: number): number {
	return Math.min(1, Math.max(0, rms * 6));
}

export function getAudioExtension(mimeType: string): string {
	const mime = mimeType.toLowerCase();
	if (mime.includes('webm')) return 'webm';
	if (mime.includes('ogg')) return 'ogg';
	if (mime.includes('mp4') || mime.includes('x-m4a')) return 'm4a';
	if (mime.includes('mpeg')) return 'mp3';
	if (mime.includes('wav')) return 'wav';
	return 'webm';
}

/**
 * Prefer the MIME reported by MediaRecorder, then the type on a real chunk.
 * The requested type is only a fallback because some WebViews leave
 * MediaRecorder.mimeType empty even when an explicit type was accepted.
 */
export function resolveRecordedMimeType(
	recorderMimeType: string | undefined,
	chunks: readonly Blob[],
	requestedMimeType?: string
): string {
	return (
		[recorderMimeType, chunks.find((chunk) => chunk.type)?.type, requestedMimeType]
			.find((mimeType) => typeof mimeType === 'string' && mimeType.trim())
			?.trim() || 'audio/webm'
	);
}

export function isAbortError(error: unknown): boolean {
	if (error instanceof Error && error.name === 'AbortError') return true;
	return typeof DOMException !== 'undefined' && error instanceof DOMException && error.name === 'AbortError';
}

export class RecordedSttProviderEmptyError extends Error {
	constructor(message: string) {
		super(message);
		this.name = 'RecordedSttProviderEmptyError';
	}
}

function isProviderEmptyError(error: unknown): boolean {
	return (
		error instanceof RecordedSttProviderEmptyError ||
		(error instanceof Error && /returned an empty transcription/i.test(error.message))
	);
}

function logMediaAccessFailure(error: unknown): void {
	console.warn('[STT] microphone-error', getMediaAccessErrorDetails('microphone', error));
}

export function formatNoSpeechMessage(diagnostics: RecordedSttDiagnostics): string {
	const displayPercent = Math.round(displayLevelFromRms(diagnostics.peakRms) * 100);
	const threshold = diagnostics.speechThreshold;
	const metrics = [
		`Peak input: ${displayPercent}%`,
		`Raw peak RMS: ${diagnostics.peakRms.toFixed(3)}`,
		...(threshold === undefined ? [] : [`Speech threshold: ${threshold.toFixed(3)}`])
	].join('; ');
	const strongSignal = threshold !== undefined && diagnostics.peakRms >= threshold * 2;

	if (strongSignal) {
		return `Microphone audio was detected, but automatic speech detection did not recognize an utterance. ${metrics} Try manual mode to isolate the recorder and provider.`;
	}
	return `No speech was detected by automatic speech detection. ${metrics} Try speaking closer to the microphone or use manual mode.`;
}

export function formatRecordedSttResultError(
	result: Pick<RecordedSttSessionResult, 'status' | 'error' | 'diagnostics'>
): string {
	switch (result.status) {
		case 'no-speech':
			return formatNoSpeechMessage(result.diagnostics);
		case 'recorder-empty':
			return result.diagnostics.peakRms > 0
				? 'Microphone audio was detected, but the recorder produced no audio data.'
				: 'The recorder produced no audio data.';
		case 'empty-recording':
			return 'The recorded audio was empty.';
		case 'microphone-ended':
			return result.error ?? 'Microphone disconnected while recording.';
		case 'microphone-error':
			return result.error ?? 'Microphone access failed.';
		case 'recorder-error':
			return result.error ?? 'The audio recorder failed.';
		case 'provider-empty':
			return result.error ?? 'The STT provider returned an empty transcription.';
		case 'timeout':
			return result.error ?? 'Speech transcription timed out. Please try again.';
		case 'provider-error':
			return result.error ?? 'Speech transcription failed.';
		case 'complete':
			return result.error ?? '';
	}
}

function initialDiagnostics(): RecordedSttDiagnostics {
	return {
		vadEnabled: false,
		analyserAvailable: false,
		speechDetected: false,
		currentRms: 0,
		peakRms: 0,
		speechCandidateActive: false,
		silenceDurationMs: 0,
		chunkCount: 0,
		recordedBytes: 0,
		durationMs: 0,
		providerStarted: false,
		uploadStarted: false,
		transcriptionStarted: false
	};
}

export class RecordedSttService {
	private transport: RecordedSttTransport | null = null;
	private mediaRecorder: MediaRecorder | null = null;
	private audioChunks: Blob[] = [];
	private stream: MediaStream | null = null;
	private analyser: AnalyserNode | null = null;
	private audioContext: AudioContext | null = null;
	private animFrameId: number | null = null;
	private callbacks: SpeechRecognitionCallbacks | null = null;
	private abortController: AbortController | null = null;
	private voiceActivityDetector: VoiceActivityDetector | null = null;
	private initialSilenceTimer: ReturnType<typeof setTimeout> | null = null;
	private maximumDurationTimer: ReturnType<typeof setTimeout> | null = null;

	private lifecycleState: RecordedSttLifecycleState = 'idle';
	private vadEnabled = false;
	private listening = false;
	private transcribing = false;
	private sessionId = 0;

	private recordingStartedAt: number | null = null;
	private recordingDurationMs = 0;
	private chunkCount = 0;
	private recordedBytes = 0;
	private requestedMimeType: string | undefined;
	private firstChunkMimeType: string | undefined;
	private recorderErrorMessage: string | undefined;
	private stopReason: RecordingStopReason | undefined;

	private currentRms = 0;
	private peakRms = 0;
	private noiseFloor: number | undefined;
	private speechThreshold: number | undefined;
	private speechCandidateActive = false;
	private speechDetected = false;
	private silenceDurationMs = 0;
	private vadEvent: VoiceActivityEvent | undefined;
	private analyserAvailable = false;
	private providerStarted = false;
	private uploadStarted = false;
	private transcriptionStarted = false;
	private providerStage: 'upload' | 'transcription' | undefined;
	private diagnostics: RecordedSttDiagnostics = initialDiagnostics();
	private lastSessionResult: RecordedSttSessionResult | null = null;

	configure(transport: RecordedSttTransport | null): void {
		this.transport = transport;
	}

	isSupported(): boolean {
		return (
			typeof navigator !== 'undefined' &&
			!!navigator.mediaDevices?.getUserMedia &&
			typeof MediaRecorder !== 'undefined'
		);
	}

	getIsListening(): boolean {
		return this.listening;
	}

	getIsTranscribing(): boolean {
		return this.transcribing;
	}

	getState(): RecordedSttLifecycleState {
		return this.lifecycleState;
	}

	getDiagnostics(): RecordedSttDiagnostics {
		return this.diagnostics;
	}

	getLastSessionResult(): RecordedSttSessionResult | null {
		return this.lastSessionResult;
	}

	async startListening(
		callbacks: SpeechRecognitionCallbacks,
		options: RecordedSttStartOptions = {}
	): Promise<boolean> {
		if (this.listening) return true;
		if (this.transcribing) return false;
		if (!this.transport) {
			this.lifecycleState = 'error';
			callbacks.onError('Speech-to-text is not configured. Set it up in Settings > Voice Input.');
			return false;
		}
		if (!this.isSupported()) {
			this.lifecycleState = 'error';
			const message = getMediaErrorMessage('microphone', { name: 'NotSupportedError' });
			this.lastSessionResult = this.makeResult(
				'microphone-error',
				this.diagnostics,
				message,
				undefined,
				getMediaAccessErrorDetails('microphone', { name: 'NotSupportedError' })
			);
			callbacks.onSessionResult?.(this.lastSessionResult);
			callbacks.onError(message);
			return false;
		}

		const sessionId = ++this.sessionId;
		this.resetSessionMetrics();
		this.callbacks = callbacks;
		this.lifecycleState = 'requesting-microphone';
		console.debug('[STT] session-start', {
			sessionId,
			mode: options.autoStop === false ? 'manual' : 'auto'
		});

		let stream: MediaStream;
		try {
			stream = await navigator.mediaDevices.getUserMedia({ audio: true });
		} catch (error) {
			if (sessionId !== this.sessionId) return false;
			logMediaAccessFailure(error);
			this.lifecycleState = 'error';
			const message = getMediaErrorMessage('microphone', error);
			const result = this.makeResult(
				'microphone-error',
				this.buildDiagnostics(currentTime()),
				message,
				undefined,
				getMediaAccessErrorDetails('microphone', error)
			);
			this.lastSessionResult = result;
			this.diagnostics = result.diagnostics;
			this.callbacks = null;
			callbacks.onSessionResult?.(result);
			callbacks.onError(message);
			console.debug('[STT] session-end', { sessionId, status: 'microphone-error' });
			return false;
		}

		console.debug('[STT] microphone-granted', { sessionId, trackCount: stream.getTracks().length });

		// Cancellation can happen while getUserMedia is pending. Release a stream
		// that arrives after that cancellation instead of leaving the mic active.
		if (sessionId !== this.sessionId) {
			stream.getTracks().forEach((track) => track.stop());
			return false;
		}

		this.stream = stream;
		this.stream.getTracks().forEach((track) => {
			track.onended = () => {
				if (sessionId !== this.sessionId || !this.listening) return;
				console.warn('[STT] microphone-ended', { sessionId });
				this.requestStop('microphone-ended');
			};
		});

		this.setupAudioAnalysis(sessionId);

		const mimeType = this.getSupportedMimeType();
		this.requestedMimeType = mimeType;
		try {
			this.mediaRecorder = mimeType
				? new MediaRecorder(this.stream, { mimeType })
				: new MediaRecorder(this.stream);
		} catch (error) {
			const message =
				error instanceof Error ? `Audio recording failed: ${error.message}` : 'Audio recording not supported on this platform';
			const result = this.makeResult('recorder-error', this.buildDiagnostics(currentTime()), message);
			this.lifecycleState = 'error';
			this.callbacks = null;
			this.cleanup();
			this.lastSessionResult = result;
			this.diagnostics = result.diagnostics;
			callbacks.onSessionResult?.(result);
			callbacks.onError(formatRecordedSttResultError(result));
			return false;
		}

		const recorder = this.mediaRecorder;
		recorder.ondataavailable = (event) => {
			if (sessionId !== this.sessionId) return;
			console.debug('[STT recorder] dataavailable', {
				size: event.data.size,
				type: event.data.type || recorder.mimeType || 'unknown'
			});
			if (event.data.size > 0) {
				this.audioChunks.push(event.data);
				this.chunkCount++;
				this.recordedBytes += event.data.size;
				this.firstChunkMimeType ||= event.data.type || undefined;
				this.publishDiagnostics(sessionId);
			}
		};
		recorder.onerror = (event) => {
			if (sessionId !== this.sessionId) return;
			const recorderError = (event as Event & { error?: { name?: string; message?: string } }).error;
			this.recorderErrorMessage = recorderError?.message
				? `Audio recorder error: ${recorderError.message}`
				: 'The audio recorder failed.';
			console.error('[STT recorder] error', {
				name: recorderError?.name,
				message: recorderError?.message
			});
			if (this.listening) this.requestStop('recorder-error');
			else this.stopReason = 'recorder-error';
		};
		recorder.onstop = () => {
			void this.handleRecordingStop(sessionId);
		};

		try {
			recorder.start(250);
			this.recordingStartedAt = currentTime();
			this.listening = true;
			this.lifecycleState = 'recording';
			this.startVoiceActivity(sessionId, options);
			this.publishDiagnostics(sessionId);
			return true;
		} catch (error) {
			const message =
				error instanceof Error ? `Failed to start audio recording: ${error.message}` : 'Failed to start audio recording';
			const result = this.makeResult('recorder-error', this.buildDiagnostics(currentTime()), message);
			this.lifecycleState = 'error';
			this.callbacks = null;
			this.cleanup();
			this.lastSessionResult = result;
			this.diagnostics = result.diagnostics;
			callbacks.onSessionResult?.(result);
			callbacks.onError(formatRecordedSttResultError(result));
			return false;
		}
	}

	stopListening(): void {
		this.requestStop('manual');
	}

	abort(): void {
		const hadActiveSession = this.listening || this.transcribing || this.lifecycleState === 'requesting-microphone';
		const sessionId = this.sessionId;
		++this.sessionId;
		this.abortController?.abort();
		this.abortController = null;
		this.callbacks = null;
		this.cleanup();
		this.listening = false;
		this.transcribing = false;
		this.lifecycleState = hadActiveSession ? 'cancelled' : 'idle';
		if (hadActiveSession) console.debug('[STT] session-end', { sessionId, status: 'cancelled' });
	}

	private setupAudioAnalysis(sessionId: number): void {
		this.analyserAvailable = false;
		if (typeof AudioContext === 'undefined' || !this.stream) {
			console.debug('[STT] analyser-unavailable', { sessionId, reason: 'AudioContext unavailable' });
			return;
		}

		try {
			this.audioContext = new AudioContext();
			const source = this.audioContext.createMediaStreamSource(this.stream);
			this.analyser = this.audioContext.createAnalyser();
			this.analyser.fftSize = 256;
			source.connect(this.analyser);
			this.analyserAvailable = true;
			console.debug('[STT] audio-analysis-ready', {
				sessionId,
				state: this.audioContext.state,
				fftSize: this.analyser.fftSize
			});
			void this.audioContext.resume().then(() => {
				if (sessionId === this.sessionId && this.audioContext) {
					console.debug('[STT] audio-context-resumed', { sessionId, state: this.audioContext.state });
				}
			}).catch(() => {
				// Recording and VAD can continue if the WebView keeps the context suspended.
			});
		} catch (error) {
			console.warn('[STT] analyser-unavailable', {
				sessionId,
				error: error instanceof Error ? error.message : 'Audio analyser creation failed'
			});
			void this.audioContext?.close().catch(() => undefined);
			this.audioContext = null;
			this.analyser = null;
		}
	}

	private startVoiceActivity(sessionId: number, options: RecordedSttStartOptions): void {
		this.vadEnabled =
			options.autoStop !== false &&
			this.analyserAvailable &&
			typeof requestAnimationFrame !== 'undefined';
		const detector = this.vadEnabled ? new VoiceActivityDetector() : null;
		detector?.start(currentTime());
		this.voiceActivityDetector = detector;
		this.refreshVadDiagnostics(detector, currentTime());

		if (this.vadEnabled) {
			this.initialSilenceTimer = setTimeout(() => {
				if (sessionId !== this.sessionId || !this.listening || detector?.hasDetectedSpeech) return;
				this.vadEvent = 'initial-silence';
				console.debug('[STT] vad-initial-silence', { sessionId, peakRms: roundRms(this.peakRms) });
				this.requestStop('initial-silence', true);
			}, DEFAULT_INITIAL_SILENCE_MS);
		}

		this.maximumDurationTimer = setTimeout(() => {
			if (sessionId !== this.sessionId || !this.listening) return;
			const discard = this.vadEnabled && !detector?.hasDetectedSpeech;
			this.vadEvent = 'maximum-duration';
			console.debug('[STT] maximum-duration', { sessionId, discard, peakRms: roundRms(this.peakRms) });
			this.requestStop('maximum-duration', discard);
		}, DEFAULT_MAX_RECORDING_MS);

		this.startLevelMonitoring(sessionId);
	}

	private processVoiceActivity(sessionId: number, rms: number, now: number): void {
		this.currentRms = Number.isFinite(rms) ? Math.max(0, rms) : 0;
		this.peakRms = Math.max(this.peakRms, this.currentRms);
		const detector = this.voiceActivityDetector;
		if (detector && sessionId === this.sessionId) {
			const event = detector.update(this.currentRms, now);
			this.refreshVadDiagnostics(detector, now);
			if (event) {
				console.debug(`[STT] vad-${event}`, {
					sessionId,
					rms: roundRms(this.currentRms),
					peakRms: roundRms(this.peakRms),
					noiseFloor: roundRms(this.noiseFloor ?? 0),
					speechThreshold: roundRms(this.speechThreshold ?? 0)
				});
			}

			switch (event) {
				case 'speech-start':
					if (this.initialSilenceTimer !== null) {
						clearTimeout(this.initialSilenceTimer);
						this.initialSilenceTimer = null;
					}
					break;
				case 'speech-end':
					this.requestStop('speech-end');
					break;
				case 'initial-silence':
					this.requestStop('initial-silence', true);
					break;
				case 'maximum-duration':
					this.requestStop('maximum-duration', !detector.hasDetectedSpeech);
					break;
			}
		}

		this.publishDiagnostics(sessionId);
	}

	private requestStop(reason: RecordingStopReason, discard = false): void {
		if (!this.mediaRecorder || !this.listening) return;

		if (reason === 'recorder-error' || this.stopReason === undefined) this.stopReason = reason;
		this.listening = false;
		this.clearVoiceActivityTimers();
		this.stopLevelMonitoring();
		console.debug('[STT] recorder-stop', {
			sessionId: this.sessionId,
			reason: this.stopReason,
			discard,
			chunks: this.chunkCount,
			bytes: this.recordedBytes
		});
		this.publishDiagnostics(this.sessionId);

		if (this.mediaRecorder.state !== 'inactive') {
			try {
				this.mediaRecorder.stop();
			} catch (error) {
				this.stopReason = 'recorder-error';
				this.recorderErrorMessage = error instanceof Error ? error.message : 'The audio recorder failed.';
				void this.handleRecordingStop(this.sessionId);
			}
		}
	}

	private getSupportedMimeType(): string | undefined {
		if (typeof MediaRecorder === 'undefined' || typeof MediaRecorder.isTypeSupported !== 'function') return undefined;
		// mp4/m4a first for Safari/WKWebView, then webm for Chromium.
		const types = ['audio/mp4', 'audio/webm;codecs=opus', 'audio/webm', 'audio/ogg;codecs=opus'];
		for (const type of types) {
			if (MediaRecorder.isTypeSupported(type)) return type;
		}
		return undefined;
	}

	private startLevelMonitoring(sessionId: number): void {
		if (!this.analyser || typeof requestAnimationFrame === 'undefined') return;

		const dataArray = new Uint8Array(this.analyser.fftSize);
		const tick = () => {
			if (!this.analyser || !this.listening || sessionId !== this.sessionId) return;
			this.analyser.getByteTimeDomainData(dataArray);
			const level = calculateRms(dataArray);
			this.processVoiceActivity(sessionId, level, currentTime());
			if (!this.listening || sessionId !== this.sessionId) return;
			this.callbacks?.onAudioLevel?.(level);
			this.animFrameId = requestAnimationFrame(tick);
		};
		this.animFrameId = requestAnimationFrame(tick);
	}

	private async handleRecordingStop(sessionId: number): Promise<void> {
		if (sessionId !== this.sessionId || (!this.mediaRecorder && !this.listening)) return;

		this.listening = false;
		this.clearVoiceActivityTimers();
		this.stopLevelMonitoring();

		const recorder = this.mediaRecorder;
		const chunks = this.audioChunks;
		const callbacks = this.callbacks;
		const transport = this.transport;
		const stopReason = this.stopReason ?? 'manual';
		const actualMime = resolveRecordedMimeType(recorder?.mimeType, chunks, this.requestedMimeType);
		this.recordingDurationMs =
			this.recordingStartedAt === null ? 0 : Math.max(0, currentTime() - this.recordingStartedAt);
		const diagnostics = this.buildDiagnostics(currentTime(), actualMime);
		this.diagnostics = diagnostics;

		this.releaseStream();
		this.detachRecorder(recorder);
		this.mediaRecorder = null;
		this.audioChunks = [];
		this.recordingStartedAt = null;

		console.debug('[STT] recorder-summary', {
			sessionId,
			stopReason,
			chunks: diagnostics.chunkCount,
			bytes: diagnostics.recordedBytes,
			blobBytes: chunks.reduce((sum, chunk) => sum + chunk.size, 0),
			mimeType: actualMime,
			durationMs: Math.round(diagnostics.durationMs),
			peakRms: roundRms(diagnostics.peakRms)
		});

		if (!callbacks || !transport) {
			this.transcribing = false;
			this.callbacks = null;
			return;
		}

		const noSpeechStop =
			this.vadEnabled &&
			!this.speechDetected &&
			(stopReason === 'initial-silence' || stopReason === 'maximum-duration');
		// A live analyser signal plus zero usable recorder chunks identifies a
		// recorder failure even when VAD also stopped for initial silence.
		if (chunks.length === 0 && (!noSpeechStop || diagnostics.peakRms > 0)) {
			const result = this.makeResult('recorder-empty', diagnostics);
			this.finishWithoutProvider(sessionId, callbacks, result, 'error');
			return;
		}
		if (noSpeechStop) {
			const result = this.makeResult('no-speech', diagnostics, formatNoSpeechMessage(diagnostics));
			this.finishWithoutProvider(sessionId, callbacks, result, 'end');
			return;
		}

		if (stopReason === 'microphone-ended') {
			const result = this.makeResult(
				'microphone-ended',
				diagnostics,
				'Microphone disconnected. Check that the device is still connected.'
			);
			this.finishWithoutProvider(sessionId, callbacks, result, 'error');
			return;
		}

		if (stopReason === 'recorder-error') {
			const result = this.makeResult('recorder-error', diagnostics, this.recorderErrorMessage);
			this.finishWithoutProvider(sessionId, callbacks, result, 'error');
			return;
		}

		if (chunks.length === 0) {
			const result = this.makeResult('recorder-empty', diagnostics);
			this.finishWithoutProvider(sessionId, callbacks, result, 'error');
			return;
		}

		const audioBlob = new Blob(chunks, { type: actualMime });
		if (audioBlob.size === 0) {
			const result = this.makeResult('empty-recording', { ...diagnostics, recordedBytes: 0 });
			this.finishWithoutProvider(sessionId, callbacks, result, 'error');
			return;
		}

		this.transcribing = true;
		this.lifecycleState = 'transcribing';
		this.providerStarted = true;
		this.publishDiagnostics(sessionId, actualMime);
		callbacks.onTranscriptionStart?.();
		console.debug('[STT] transcription-start', {
			sessionId,
			bytes: audioBlob.size,
			mimeType: actualMime,
			filename: `recording.${getAudioExtension(actualMime)}`,
			stopReason
		});

		const controller = new AbortController();
		this.abortController = controller;
		let timedOut = false;
		const timeoutId = setTimeout(() => {
			timedOut = true;
			controller.abort();
		}, transport.timeoutMs ?? DEFAULT_RECORDED_STT_TIMEOUT_MS);

		try {
			const text = await transport.transcribe(audioBlob, {
				filename: `recording.${getAudioExtension(actualMime)}`,
				signal: controller.signal,
				onStage: (stage) => this.handleTransportStage(sessionId, stage)
			});

			if (sessionId !== this.sessionId) return;
			if (controller.signal.aborted) {
				if (timedOut) {
					const result = this.makeResult(
						'timeout',
						this.buildDiagnostics(currentTime(), actualMime),
						transport.timeoutMessage
					);
					this.finishWithoutProvider(sessionId, callbacks, result, 'error');
				}
				return;
			}

			const finalText = text.trim();
			if (!finalText) {
				const result = this.makeResult(
					'provider-empty',
					this.buildDiagnostics(currentTime(), actualMime),
					transport.emptyResultMessage ?? 'The STT provider returned an empty transcription.'
				);
				this.finishWithoutProvider(sessionId, callbacks, result, 'error');
				return;
			}

			this.transcriptionStarted = true;
			this.lifecycleState = 'complete';
			this.transcribing = false;
			const result = this.makeResult(
				'complete',
				this.buildDiagnostics(currentTime(), actualMime),
				undefined,
				finalText
			);
			this.lastSessionResult = result;
			this.diagnostics = result.diagnostics;
			this.callbacks = null;
			console.debug('[STT] transcription-success', {
				sessionId,
				characters: finalText.length,
				bytes: audioBlob.size,
				mimeType: actualMime
			});
			callbacks.onSessionResult?.(result);
			callbacks.onResult(finalText, true);
			callbacks.onEnd(result);
			console.debug('[STT] session-end', { sessionId, status: result.status, stopReason });
		} catch (error) {
			if (sessionId !== this.sessionId) return;
			if (timedOut) {
				const result = this.makeResult(
					'timeout',
					this.buildDiagnostics(currentTime(), actualMime),
					transport.timeoutMessage
				);
				this.finishWithoutProvider(sessionId, callbacks, result, 'error');
			} else if (isAbortError(error)) {
				// User cancellation increments sessionId, so this branch only covers
				// a provider that aborts its own request. Do not mislabel it as VAD.
				return;
			} else {
				const message = error instanceof Error ? error.message : 'Speech transcription failed.';
				const status = isProviderEmptyError(error) ? 'provider-empty' : 'provider-error';
				const result = this.makeResult(status, this.buildDiagnostics(currentTime(), actualMime), message);
				console.error('[STT] transcription-error', {
					sessionId,
					stage: this.providerStage,
					status,
					message
				});
				this.finishWithoutProvider(sessionId, callbacks, result, 'error');
			}
		} finally {
			clearTimeout(timeoutId);
			if (sessionId === this.sessionId) this.abortController = null;
		}
	}

	private finishWithoutProvider(
		sessionId: number,
		callbacks: SpeechRecognitionCallbacks,
		result: RecordedSttSessionResult,
		delivery: 'end' | 'error'
	): void {
		if (sessionId !== this.sessionId) return;
		this.transcribing = false;
		this.lifecycleState = 'error';
		this.lastSessionResult = result;
		this.diagnostics = result.diagnostics;
		this.callbacks = null;
		callbacks.onSessionResult?.(result);
		if (delivery === 'end') callbacks.onEnd(result);
		else callbacks.onError(formatRecordedSttResultError(result));
		console.debug('[STT] session-end', {
			sessionId,
			status: result.status,
			stopReason: result.diagnostics.stopReason
		});
	}

	private handleTransportStage(sessionId: number, stage: SttTransportStage): void {
		if (sessionId !== this.sessionId) return;
		switch (stage) {
			case 'upload-start':
				this.uploadStarted = true;
				this.providerStage = 'upload';
				break;
			case 'upload-success':
				this.uploadStarted = true;
				this.providerStage = 'upload';
				break;
			case 'transcription-start':
				this.transcriptionStarted = true;
				this.providerStage = 'transcription';
				break;
			case 'transcription-success':
				this.transcriptionStarted = true;
				this.providerStage = 'transcription';
				break;
		}
		this.publishDiagnostics(sessionId);
	}

	private refreshVadDiagnostics(detector: VoiceActivityDetector | null, now: number): void {
		if (!detector) {
			this.noiseFloor = undefined;
			this.speechThreshold = undefined;
			this.speechCandidateActive = false;
			this.silenceDurationMs = 0;
			this.vadEvent = undefined;
			this.speechDetected = false;
			return;
		}
		const diagnostics = detector.getDiagnostics(now);
		this.currentRms = diagnostics.currentRms;
		this.peakRms = Math.max(this.peakRms, diagnostics.peakRms);
		this.noiseFloor = diagnostics.noiseFloor;
		this.speechThreshold = diagnostics.speechThreshold;
		this.speechCandidateActive = diagnostics.speechCandidateActive;
		this.speechDetected = diagnostics.speechDetected;
		this.silenceDurationMs = diagnostics.silenceDurationMs;
		this.vadEvent = diagnostics.vadEvent;
	}

	private buildDiagnostics(now: number, mimeType?: string): RecordedSttDiagnostics {
		return {
			stopReason: this.stopReason,
			vadEnabled: this.vadEnabled,
			analyserAvailable: this.analyserAvailable,
			speechDetected: this.speechDetected,
			currentRms: this.currentRms,
			peakRms: this.peakRms,
			noiseFloor: this.noiseFloor,
			speechThreshold: this.speechThreshold,
			speechCandidateActive: this.speechCandidateActive,
			silenceDurationMs: this.silenceDurationMs,
			vadEvent: this.vadEvent,
			mimeType: mimeType || this.mediaRecorder?.mimeType || this.firstChunkMimeType || this.requestedMimeType,
			chunkCount: this.chunkCount,
			recordedBytes: this.recordedBytes,
			durationMs:
				this.recordingStartedAt === null
					? this.recordingDurationMs
					: Math.max(0, now - this.recordingStartedAt),
			providerStarted: this.providerStarted,
			uploadStarted: this.uploadStarted,
			transcriptionStarted: this.transcriptionStarted,
			providerStage: this.providerStage
		};
	}

	private makeResult(
		status: RecordedSttResultStatus,
		diagnostics: RecordedSttDiagnostics,
		error?: string,
		text?: string,
		mediaError?: MediaAccessErrorDetails
	): RecordedSttSessionResult {
		return {
			status,
			diagnostics,
			...(error ? { error } : {}),
			...(text ? { text } : {}),
			...(mediaError ? { mediaError } : {})
		};
	}

	private publishDiagnostics(sessionId: number, mimeType?: string): void {
		if (sessionId !== this.sessionId) return;
		this.diagnostics = this.buildDiagnostics(currentTime(), mimeType);
		this.callbacks?.onDiagnostics?.(this.diagnostics);
	}

	private detachRecorder(recorder: MediaRecorder | null): void {
		if (!recorder) return;
		recorder.ondataavailable = null;
		recorder.onstop = null;
		recorder.onerror = null;
	}

	private stopLevelMonitoring(): void {
		if (this.animFrameId !== null && typeof cancelAnimationFrame !== 'undefined') {
			cancelAnimationFrame(this.animFrameId);
		}
		this.animFrameId = null;
	}

	private releaseStream(): void {
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

	private cleanup(): void {
		this.clearVoiceActivityTimers();
		this.stopLevelMonitoring();
		const recorder = this.mediaRecorder;
		this.mediaRecorder = null;
		this.detachRecorder(recorder);
		if (recorder && recorder.state !== 'inactive') {
			try {
				recorder.stop();
			} catch {
				// The session is already being cancelled or torn down.
			}
		}
		this.audioChunks = [];
		this.recordingStartedAt = null;
		this.releaseStream();
		this.resetSessionMetrics();
	}

	private resetSessionMetrics(): void {
		this.audioChunks = [];
		this.recordingStartedAt = null;
		this.recordingDurationMs = 0;
		this.chunkCount = 0;
		this.recordedBytes = 0;
		this.requestedMimeType = undefined;
		this.firstChunkMimeType = undefined;
		this.recorderErrorMessage = undefined;
		this.stopReason = undefined;
		this.currentRms = 0;
		this.peakRms = 0;
		this.noiseFloor = undefined;
		this.speechThreshold = undefined;
		this.speechCandidateActive = false;
		this.speechDetected = false;
		this.silenceDurationMs = 0;
		this.vadEvent = undefined;
		this.analyserAvailable = false;
		this.vadEnabled = false;
		this.providerStarted = false;
		this.uploadStarted = false;
		this.transcriptionStarted = false;
		this.providerStage = undefined;
		this.diagnostics = initialDiagnostics();
		this.lastSessionResult = null;
	}

	private clearVoiceActivityTimers(): void {
		if (this.initialSilenceTimer !== null) {
			clearTimeout(this.initialSilenceTimer);
			this.initialSilenceTimer = null;
		}
		if (this.maximumDurationTimer !== null) {
			clearTimeout(this.maximumDurationTimer);
			this.maximumDurationTimer = null;
		}
		this.voiceActivityDetector?.reset();
		this.voiceActivityDetector = null;
	}
}

/** One shared recorder used by every recorded STT transport. */
export const recordedSttService = new RecordedSttService();
