import type { SpeechRecognitionCallbacks } from './web-speech.ts';
import { getMediaAccessErrorDetails, getMediaErrorMessage } from '../media/media-errors.ts';
import {
	calculateRms,
	DEFAULT_INITIAL_SILENCE_MS,
	DEFAULT_MAX_RECORDING_MS,
	VoiceActivityDetector
} from '../media/voice-activity.ts';

export interface SttTransportContext {
	signal: AbortSignal;
	filename: string;
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
}

export const DEFAULT_RECORDED_STT_TIMEOUT_MS = 30_000;

export interface RecordedSttStartOptions {
	/** Disable automatic end-of-speech so a diagnostic can isolate the provider. */
	autoStop?: boolean;
}

export function getAudioExtension(mimeType: string): string {
	const mime = mimeType.toLowerCase();
	if (mime.includes('webm')) return 'webm';
	if (mime.includes('ogg')) return 'ogg';
	if (mime.includes('mp4')) return 'm4a';
	if (mime.includes('mpeg')) return 'mp3';
	if (mime.includes('wav')) return 'wav';
	return 'webm';
}

export function isAbortError(error: unknown): boolean {
	if (error instanceof Error && error.name === 'AbortError') return true;
	return typeof DOMException !== 'undefined' && error instanceof DOMException && error.name === 'AbortError';
}

function logMediaAccessFailure(error: unknown): void {
	console.warn('[RecordedSttService] getUserMedia failed', getMediaAccessErrorDetails('microphone', error));
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
	private vadEnabled = false;
	private discardRecording = false;
	private listening = false;
	private transcribing = false;
	private sessionId = 0;

	configure(transport: RecordedSttTransport | null): void {
		this.transport = transport;
	}

	isSupported(): boolean {
		return typeof navigator !== 'undefined' && !!navigator.mediaDevices?.getUserMedia;
	}

	getIsListening(): boolean {
		return this.listening;
	}

	getIsTranscribing(): boolean {
		return this.transcribing;
	}

	async startListening(
		callbacks: SpeechRecognitionCallbacks,
		options: RecordedSttStartOptions = {}
	): Promise<boolean> {
		if (this.listening) return true;
		if (!this.transport) {
			callbacks.onError('Speech-to-text is not configured. Set it up in Settings > Voice Input.');
			return false;
		}
		if (!this.isSupported()) {
			callbacks.onError(getMediaErrorMessage('microphone', { name: 'NotSupportedError' }));
			return false;
		}

		const sessionId = ++this.sessionId;
		this.callbacks = callbacks;
		this.audioChunks = [];
		this.clearVoiceActivityTimers();
		this.discardRecording = false;

		let stream: MediaStream;
		try {
			stream = await navigator.mediaDevices.getUserMedia({ audio: true });
		} catch (error) {
			if (sessionId !== this.sessionId) return false;
			logMediaAccessFailure(error);
			callbacks.onError(getMediaErrorMessage('microphone', error));
			this.callbacks = null;
			return false;
		}

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
				const currentCallbacks = this.callbacks;
				this.sessionId++;
				this.callbacks = null;
				this.listening = false;
				this.transcribing = false;
				this.cleanup();
				currentCallbacks?.onError('Microphone disconnected');
			};
		});

		// Set up audio analysis for real levels where AudioContext is available.
		if (typeof AudioContext !== 'undefined') {
			try {
				this.audioContext = new AudioContext();
				const source = this.audioContext.createMediaStreamSource(this.stream);
				this.analyser = this.audioContext.createAnalyser();
				this.analyser.fftSize = 256;
				source.connect(this.analyser);
				void this.audioContext.resume().catch(() => {
					// Recording and VAD can continue if the webview keeps the context suspended.
				});
			} catch {
				// Recording can continue without level monitoring on restricted webviews.
				this.audioContext?.close();
				this.audioContext = null;
				this.analyser = null;
			}
		}

		const mimeType = this.getSupportedMimeType();
		try {
			this.mediaRecorder = mimeType
				? new MediaRecorder(this.stream, { mimeType })
				: new MediaRecorder(this.stream);
		} catch {
			this.callbacks = null;
			this.cleanup();
			callbacks.onError('Audio recording not supported on this platform');
			return false;
		}

		this.mediaRecorder.ondataavailable = (event) => {
			if (sessionId === this.sessionId && event.data.size > 0) {
				this.audioChunks.push(event.data);
			}
		};
		this.mediaRecorder.onstop = () => {
			void this.handleRecordingStop(sessionId);
		};

		try {
			this.mediaRecorder.start(250);
			this.listening = true;
			this.startVoiceActivity(sessionId, options);
			return true;
		} catch {
			this.callbacks = null;
			this.cleanup();
			callbacks.onError('Failed to start audio recording');
			return false;
		}
	}

	stopListening(): void {
		this.requestStop(false);
	}

	abort(): void {
		this.sessionId++;
		this.abortController?.abort();
		this.abortController = null;
		this.callbacks = null;
		this.cleanup();
		this.listening = false;
		this.transcribing = false;
	}

	private startVoiceActivity(sessionId: number, options: RecordedSttStartOptions): void {
		this.vadEnabled = options.autoStop !== false && !!this.analyser;
		const detector = this.vadEnabled ? new VoiceActivityDetector() : null;
		detector?.start(performance.now());
		this.voiceActivityDetector = detector;

		if (this.vadEnabled) {
			this.initialSilenceTimer = setTimeout(() => {
				if (sessionId !== this.sessionId || !this.listening || detector?.hasDetectedSpeech) return;
				this.requestStop(true);
			}, DEFAULT_INITIAL_SILENCE_MS);
		}

		this.maximumDurationTimer = setTimeout(() => {
			if (sessionId !== this.sessionId || !this.listening) return;
			this.requestStop(this.vadEnabled && !detector?.hasDetectedSpeech);
		}, DEFAULT_MAX_RECORDING_MS);

		this.startLevelMonitoring(sessionId);
	}

	private processVoiceActivity(sessionId: number, rms: number, now: number): void {
		const detector = this.voiceActivityDetector;
		if (!detector || sessionId !== this.sessionId) return;

		const event = detector.update(rms, now);
		if (event) {
			console.debug('[RecordedSttService] voice activity', {
				event,
				rms: Number(rms.toFixed(4))
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
				this.requestStop(false);
				break;
			case 'initial-silence':
				this.requestStop(true);
				break;
			case 'maximum-duration':
				this.requestStop(this.vadEnabled && !detector.hasDetectedSpeech);
				break;
		}
	}

	private requestStop(discard: boolean): void {
		if (!this.mediaRecorder || !this.listening) return;

		this.discardRecording ||= discard;
		this.listening = false;
		this.clearVoiceActivityTimers();
		this.stopLevelMonitoring();
		if (this.mediaRecorder.state !== 'inactive') this.mediaRecorder.stop();
	}

	private getSupportedMimeType(): string | undefined {
		// mp4/m4a first for Safari/WKWebView, then webm for Chromium.
		const types = ['audio/mp4', 'audio/webm;codecs=opus', 'audio/webm', 'audio/ogg;codecs=opus'];
		for (const type of types) {
			if (MediaRecorder.isTypeSupported(type)) return type;
		}
		return undefined;
	}

	private startLevelMonitoring(sessionId: number): void {
		if (!this.analyser) return;

		const dataArray = new Uint8Array(this.analyser.fftSize);
		const tick = () => {
			if (!this.analyser || !this.listening || sessionId !== this.sessionId) return;
			this.analyser.getByteTimeDomainData(dataArray);
			const level = calculateRms(dataArray);
			this.processVoiceActivity(sessionId, level, performance.now());
			if (!this.listening || sessionId !== this.sessionId) return;
			this.callbacks?.onAudioLevel?.(level);
			this.animFrameId = requestAnimationFrame(tick);
		};
		this.animFrameId = requestAnimationFrame(tick);
	}

	private async handleRecordingStop(sessionId: number): Promise<void> {
		if (sessionId !== this.sessionId) return;

		this.listening = false;
		this.clearVoiceActivityTimers();
		this.stopLevelMonitoring();
		this.releaseStream();

		const discardRecording = this.discardRecording;
		this.discardRecording = false;
		const callbacks = this.callbacks;
		const transport = this.transport;
		if (discardRecording) {
			this.audioChunks = [];
			this.mediaRecorder = null;
			this.callbacks = null;
			callbacks?.onEnd();
			return;
		}
		if (!callbacks || !transport) {
			this.audioChunks = [];
			this.mediaRecorder = null;
			this.callbacks = null;
			return;
		}

		if (this.audioChunks.length === 0) {
			this.mediaRecorder = null;
			this.callbacks = null;
			callbacks.onEnd();
			return;
		}

		this.transcribing = true;
		callbacks.onTranscriptionStart?.();
		const actualMime = this.mediaRecorder?.mimeType || this.audioChunks.find((chunk) => chunk.type)?.type || 'audio/webm';
		const audioBlob = new Blob(this.audioChunks, { type: actualMime });
		this.audioChunks = [];
		this.mediaRecorder = null;

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
				signal: controller.signal
			});

			if (sessionId !== this.sessionId) return;
			if (controller.signal.aborted) {
				this.transcribing = false;
				this.callbacks = null;
				if (timedOut) {
					callbacks.onError(transport.timeoutMessage ?? 'Speech transcription timed out. Please try again.');
				}
				return;
			}
			this.transcribing = false;
			this.callbacks = null;
			const finalText = text.trim();
			if (finalText) callbacks.onResult(finalText, true);
			callbacks.onEnd();
		} catch (error) {
			if (sessionId !== this.sessionId) return;
			this.transcribing = false;
			this.callbacks = null;
			if (timedOut) {
				callbacks.onError(transport.timeoutMessage ?? 'Speech transcription timed out. Please try again.');
			} else if (!isAbortError(error)) {
				callbacks.onError(error instanceof Error ? error.message : 'Speech transcription failed');
			}
		} finally {
			clearTimeout(timeoutId);
			if (sessionId === this.sessionId) this.abortController = null;
		}
	}

	private stopLevelMonitoring(): void {
		if (this.animFrameId !== null) {
			cancelAnimationFrame(this.animFrameId);
			this.animFrameId = null;
		}
	}

	private releaseStream(): void {
		if (this.stream) {
			this.stream.getTracks().forEach((track) => track.stop());
			this.stream = null;
		}
		if (this.audioContext) {
			void this.audioContext.close();
			this.audioContext = null;
		}
		this.analyser = null;
	}

	private cleanup(): void {
		this.clearVoiceActivityTimers();
		this.stopLevelMonitoring();
		if (this.mediaRecorder && this.mediaRecorder.state !== 'inactive') {
			this.mediaRecorder.stop();
		}
		this.mediaRecorder = null;
		this.audioChunks = [];
		this.discardRecording = false;
		this.releaseStream();
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
		this.vadEnabled = false;
	}
}

/** One shared recorder used by every recorded STT transport. */
export const recordedSttService = new RecordedSttService();
