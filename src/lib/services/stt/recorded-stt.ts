import type { SpeechRecognitionCallbacks } from './web-speech.ts';

export interface SttTransportContext {
	signal: AbortSignal;
	filename: string;
}

/**
 * Provider-independent transport for push-to-talk recordings.
 *
 * The recorder owns microphone permissions, MediaRecorder, audio levels, and
 * cancellation. Providers only need to turn the finished Blob into text.
 */
export interface RecordedSttTransport {
	transcribe(audio: Blob, context: SttTransportContext): Promise<string>;
	timeoutMs?: number;
	timeoutMessage?: string;
}

export const DEFAULT_RECORDED_STT_TIMEOUT_MS = 30_000;

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

	async startListening(callbacks: SpeechRecognitionCallbacks): Promise<boolean> {
		if (this.listening) return true;
		if (!this.transport) {
			callbacks.onError('Speech-to-text is not configured. Set it up in Settings > Persona.');
			return false;
		}
		if (!this.isSupported()) {
			callbacks.onError('Microphone access is not supported in this browser.');
			return false;
		}

		const sessionId = ++this.sessionId;
		this.callbacks = callbacks;
		this.audioChunks = [];

		let stream: MediaStream;
		try {
			stream = await navigator.mediaDevices.getUserMedia({ audio: true });
		} catch (error) {
			if (sessionId !== this.sessionId) return false;
			if (error instanceof DOMException) {
				const messages: Record<string, string> = {
					NotAllowedError: 'Microphone access denied. Check system permissions.',
					NotFoundError: 'No microphone found. Please connect a microphone.',
					NotReadableError: 'Microphone is busy or in use by another app.',
					OverconstrainedError: 'Microphone does not meet requirements.'
				};
				callbacks.onError(messages[error.name] || `Microphone error: ${error.message}`);
			} else {
				callbacks.onError('Failed to access microphone');
			}
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
				this.startLevelMonitoring();
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
			return true;
		} catch {
			this.callbacks = null;
			this.cleanup();
			callbacks.onError('Failed to start audio recording');
			return false;
		}
	}

	stopListening(): void {
		if (this.mediaRecorder && this.listening) {
			this.mediaRecorder.stop();
		}
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

	private getSupportedMimeType(): string | undefined {
		// mp4/m4a first for Safari/WKWebView, then webm for Chromium.
		const types = ['audio/mp4', 'audio/webm;codecs=opus', 'audio/webm', 'audio/ogg;codecs=opus'];
		for (const type of types) {
			if (MediaRecorder.isTypeSupported(type)) return type;
		}
		return undefined;
	}

	private startLevelMonitoring(): void {
		if (!this.analyser || !this.callbacks?.onAudioLevel) return;

		const dataArray = new Uint8Array(this.analyser.frequencyBinCount);
		const tick = () => {
			if (!this.analyser || !this.listening) return;
			this.analyser.getByteFrequencyData(dataArray);
			let sum = 0;
			for (let i = 0; i < dataArray.length; i++) sum += dataArray[i];
			const level = dataArray.length ? sum / (dataArray.length * 255) : 0;
			this.callbacks?.onAudioLevel?.(level);
			this.animFrameId = requestAnimationFrame(tick);
		};
		this.animFrameId = requestAnimationFrame(tick);
	}

	private async handleRecordingStop(sessionId: number): Promise<void> {
		if (sessionId !== this.sessionId) return;

		this.listening = false;
		this.stopLevelMonitoring();
		this.releaseStream();

		const callbacks = this.callbacks;
		const transport = this.transport;
		if (!callbacks || !transport) return;

		if (this.audioChunks.length === 0) {
			this.callbacks = null;
			callbacks.onEnd();
			return;
		}

		this.transcribing = true;
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
		this.stopLevelMonitoring();
		if (this.mediaRecorder && this.mediaRecorder.state !== 'inactive') {
			this.mediaRecorder.stop();
		}
		this.mediaRecorder = null;
		this.audioChunks = [];
		this.releaseStream();
	}
}

/** One shared recorder used by every recorded STT transport. */
export const recordedSttService = new RecordedSttService();
