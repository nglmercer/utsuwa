import type { SpeechRecognitionCallbacks } from './web-speech.ts';
import {
	getMediaAccessErrorDetails,
	getMediaErrorMessage,
	type MediaAccessErrorDetails
} from '../media/media-errors.ts';
import {
	DEFAULT_MAX_RECORDING_MS,
	DEFAULT_SPEECH_END_SILENCE_MS,
	type VoiceActivityEvent
} from '../media/voice-activity.ts';
import {
	createAudioCaptureBackend,
	type AudioCaptureBackend,
	type AudioCaptureDiagnostics,
	type AudioCaptureStopReason
} from '../audio/audio-capture.ts';

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
	backend?: 'native-cpal' | 'web-media-recorder';
	device?: string;
	sampleRate?: number;
	channels?: number;
	wavBytes?: number;
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
 * The selected audio backend owns microphone permissions, capture, voice
 * activity, audio levels, and cancellation. Providers only need to turn the
 * finished Blob into text. Manual stop remains available as an explicit
 * fallback.
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
	private callbacks: SpeechRecognitionCallbacks | null = null;
	private abortController: AbortController | null = null;

	private lifecycleState: RecordedSttLifecycleState = 'idle';
	private vadEnabled = false;
	private listening = false;
	private transcribing = false;
	private sessionId = 0;

	private recordingStartedAt: number | null = null;
	private recordingDurationMs = 0;
	private chunkCount = 0;
	private recordedBytes = 0;
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
	private audioBackend: AudioCaptureBackend | null = null;
	private audioStopPromise: Promise<void> | null = null;
	private audioAutoStop = true;
	private captureBackend: 'native-cpal' | 'web-media-recorder' = 'web-media-recorder';
	private captureDevice: string | undefined;
	private captureSampleRate: number | undefined;
	private captureChannels: number | undefined;
	private captureWavBytes: number | undefined;
	private captureMimeType: string | undefined;

	configure(transport: RecordedSttTransport | null): void {
		this.transport = transport;
	}

	isSupported(): boolean {
		return createAudioCaptureBackend().isSupported();
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
		const backend = createAudioCaptureBackend();
		if (!backend.isSupported()) {
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
		return this.startAudioCapture(sessionId, callbacks, options, backend);
	}

	private async startAudioCapture(
		sessionId: number,
		callbacks: SpeechRecognitionCallbacks,
		options: RecordedSttStartOptions,
		backend: AudioCaptureBackend
	): Promise<boolean> {
		this.audioBackend = backend;
		this.audioAutoStop = options.autoStop !== false;
		this.captureBackend = backend.name;
		this.vadEnabled = this.audioAutoStop;
		this.analyserAvailable = backend.name === 'native-cpal';
		this.callbacks = callbacks;

		try {
			await backend.start({
				autoStop: this.audioAutoStop,
				silenceDurationMs: DEFAULT_SPEECH_END_SILENCE_MS,
				maxDurationMs: DEFAULT_MAX_RECORDING_MS,
				onAudioLevel: (rms, peakRms) => {
					if (sessionId !== this.sessionId) return;
					this.currentRms = Number.isFinite(rms) ? Math.max(0, rms) : 0;
					this.peakRms = Math.max(this.peakRms, Number.isFinite(peakRms) ? peakRms : this.currentRms);
					this.publishDiagnostics(sessionId);
					callbacks.onAudioLevel?.(this.currentRms);
				},
				onDiagnostics: (diagnostics) => {
					if (sessionId !== this.sessionId) return;
					this.applyCaptureDiagnostics(diagnostics);
					this.publishDiagnostics(sessionId);
				},
				onStopped: (reason) => {
					this.beginAudioStop(sessionId, reason);
				}
			});
		} catch (error) {
			if (sessionId !== this.sessionId) return false;
			if (backend.name === 'web-media-recorder') logMediaAccessFailure(error);
			const message = backend.name === 'web-media-recorder'
				? getMediaErrorMessage('microphone', error)
				: error instanceof Error ? error.message : 'Native microphone capture failed.';
			const result = this.makeResult(
				'microphone-error',
				this.buildDiagnostics(currentTime()),
				message,
				undefined,
				backend.name === 'web-media-recorder' ? getMediaAccessErrorDetails('microphone', error) : undefined
			);
			this.audioBackend = null;
			this.callbacks = null;
			this.lifecycleState = 'error';
			this.lastSessionResult = result;
			this.diagnostics = result.diagnostics;
			callbacks.onSessionResult?.(result);
			callbacks.onError(formatRecordedSttResultError(result));
			return false;
		}

		if (sessionId !== this.sessionId) {
			await backend.cancel().catch(() => undefined);
			return false;
		}
		this.listening = true;
		this.recordingStartedAt = currentTime();
		this.lifecycleState = 'recording';
		this.publishDiagnostics(sessionId);
		return true;
	}

	private beginAudioStop(sessionId: number, reason: AudioCaptureStopReason): void {
		if (sessionId !== this.sessionId || !this.audioBackend || this.audioStopPromise) return;
		this.listening = false;
		this.transcribing = true;
		this.lifecycleState = 'transcribing';
		const completion = Promise.resolve().then(() => this.completeAudioCapture(sessionId, reason));
		this.audioStopPromise = completion;
		void completion
			.finally(() => {
				if (sessionId === this.sessionId) this.audioStopPromise = null;
			})
			.catch(() => undefined);
	}

	private async completeAudioCapture(
		sessionId: number,
		reason: AudioCaptureStopReason
	): Promise<void> {
		const backend = this.audioBackend;
		const callbacks = this.callbacks;
		const transport = this.transport;
		if (!backend || !callbacks || !transport) return;

		this.stopReason = this.mapCaptureStopReason(reason);
		let audioBlob: Blob;
		try {
			audioBlob = await backend.stop();
		} catch (error) {
			if (sessionId !== this.sessionId) return;
			const message = error instanceof Error ? error.message : 'Audio capture failed.';
			this.applyCaptureDiagnostics(backend.getDiagnostics());
			this.audioBackend = null;
			this.recordingStartedAt = null;
			const result = this.makeResult('recorder-error', this.buildDiagnostics(currentTime()), message);
			this.finishWithoutProvider(sessionId, callbacks, result, 'error');
			return;
		}
		if (sessionId !== this.sessionId) return;

		this.applyCaptureDiagnostics(backend.getDiagnostics());
		this.audioBackend = null;
		this.recordingStartedAt = null;
		const diagnostics = this.buildDiagnostics(currentTime());
		this.diagnostics = diagnostics;
		const noSpeechStop =
			this.vadEnabled &&
			!diagnostics.speechDetected &&
			(reason === 'silence-detected' || reason === 'maximum-duration');

		if (noSpeechStop) {
			const result = this.makeResult('no-speech', diagnostics, formatNoSpeechMessage(diagnostics));
			this.finishWithoutProvider(sessionId, callbacks, result, 'end');
			return;
		}
		if (reason === 'error') {
			const result = this.makeResult('recorder-error', diagnostics, 'The audio recorder failed.');
			this.finishWithoutProvider(sessionId, callbacks, result, 'error');
			return;
		}
		if (reason === 'microphone-ended') {
			const result = this.makeResult(
				'microphone-ended',
				diagnostics,
				'Microphone disconnected. Check that the device is still connected.'
			);
			this.finishWithoutProvider(sessionId, callbacks, result, 'error');
			return;
		}
		if (audioBlob.size === 0) {
			const result = this.makeResult(
				backend.name === 'web-media-recorder' ? 'recorder-empty' : 'empty-recording',
				diagnostics
			);
			this.finishWithoutProvider(sessionId, callbacks, result, 'error');
			return;
		}
		if (
			diagnostics.durationMs === 0 ||
			(backend.name === 'native-cpal' && (diagnostics.wavBytes ?? audioBlob.size) <= 44)
		) {
			const result = this.makeResult('recorder-empty', diagnostics);
			this.finishWithoutProvider(sessionId, callbacks, result, 'error');
			return;
		}

		await this.transcribeBlob(
			sessionId,
			callbacks,
			transport,
			audioBlob,
			diagnostics,
			diagnostics.mimeType ?? (backend.name === 'native-cpal' ? 'audio/wav' : 'audio/webm'),
			this.stopReason
		);
	}

	private applyCaptureDiagnostics(diagnostics: AudioCaptureDiagnostics): void {
		this.captureBackend = diagnostics.backend;
		this.captureDevice = diagnostics.device;
		this.captureSampleRate = diagnostics.sampleRate;
		this.captureChannels = diagnostics.channels;
		this.captureWavBytes = diagnostics.wavBytes;
		this.captureMimeType = diagnostics.mimeType;
		if (diagnostics.analyserAvailable !== undefined) this.analyserAvailable = diagnostics.analyserAvailable;
		if (diagnostics.backend === 'web-media-recorder' && diagnostics.analyserAvailable === false) {
			this.vadEnabled = false;
		}
		this.currentRms = diagnostics.currentRms;
		this.peakRms = Math.max(this.peakRms, diagnostics.peakRms);
		this.noiseFloor = diagnostics.noiseFloor;
		this.speechThreshold = diagnostics.speechThreshold;
		this.speechCandidateActive = diagnostics.speechCandidateActive;
		this.speechDetected = diagnostics.speechDetected;
		this.silenceDurationMs = diagnostics.silenceDurationMs;
		this.vadEvent = diagnostics.vadEvent;
		this.recordingDurationMs = diagnostics.durationMs;
		this.recordedBytes = diagnostics.recordedBytes;
		this.chunkCount = diagnostics.chunkCount;
	}

	private mapCaptureStopReason(reason: AudioCaptureStopReason): RecordingStopReason {
		switch (reason) {
			case 'silence-detected':
				return 'speech-end';
			case 'maximum-duration':
				return 'maximum-duration';
			case 'microphone-ended':
				return 'microphone-ended';
			case 'error':
				return 'recorder-error';
			default:
				return 'manual';
		}
	}

	stopListening(): void {
		if (!this.audioBackend || !this.listening || this.audioStopPromise) return;
		this.beginAudioStop(this.sessionId, 'manual');
	}

	abort(): void {
		const hadActiveSession = this.listening || this.transcribing || this.lifecycleState === 'requesting-microphone';
		const sessionId = this.sessionId;
		const backend = this.audioBackend;
		const stopInProgress = this.audioStopPromise !== null;
		++this.sessionId;
		this.abortController?.abort();
		this.abortController = null;
		this.callbacks = null;
		this.audioBackend = null;
		this.audioStopPromise = null;
		if (backend && !stopInProgress) void backend.cancel().catch(() => undefined);
		this.resetSessionMetrics();
		this.listening = false;
		this.transcribing = false;
		this.lifecycleState = hadActiveSession ? 'cancelled' : 'idle';
		if (hadActiveSession) console.debug('[STT] session-end', { sessionId, status: 'cancelled' });
	}

	/** Send a completed Blob through the unchanged provider transport. */
	private async transcribeBlob(
		sessionId: number,
		callbacks: SpeechRecognitionCallbacks,
		transport: RecordedSttTransport,
		audioBlob: Blob,
		diagnostics: RecordedSttDiagnostics,
		actualMime: string,
		stopReason: RecordingStopReason
	): Promise<void> {
		this.diagnostics = diagnostics;
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

	private buildDiagnostics(now: number, mimeType?: string): RecordedSttDiagnostics {
		return {
			backend: this.captureBackend,
			device: this.captureDevice,
			sampleRate: this.captureSampleRate,
			channels: this.captureChannels,
			wavBytes: this.captureWavBytes,
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
			mimeType: mimeType || this.captureMimeType,
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

	private resetSessionMetrics(): void {
		this.recordingStartedAt = null;
		this.recordingDurationMs = 0;
		this.chunkCount = 0;
		this.recordedBytes = 0;
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
		this.captureBackend = 'web-media-recorder';
		this.captureDevice = undefined;
		this.captureSampleRate = undefined;
		this.captureChannels = undefined;
		this.captureWavBytes = undefined;
		this.captureMimeType = undefined;
		this.audioAutoStop = true;
		this.audioBackend = null;
		this.audioStopPromise = null;
		this.diagnostics = initialDiagnostics();
		this.lastSessionResult = null;
	}

}

/** One shared recorder used by every recorded STT transport. */
export const recordedSttService = new RecordedSttService();
