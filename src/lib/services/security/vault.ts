/** Settings vault: passphrase-based AES-GCM encryption for secrets at rest.
 *
 * `localStorage` is readable by any script on the origin and by local
 * attackers, so provider API keys and MCP bearer tokens must not sit there in
 * plaintext once the user opts in. This module owns the envelope format and
 * the WebCrypto primitives; `settings.svelte.ts` owns persistence policy
 * (when to encrypt, locked-state behavior). The derived key lives in memory
 * only and is never persisted.
 *
 * Pure WebCrypto — no `$lib` imports — so it stays unit-testable under node.
 */

export const VAULT_VERSION = 1;
export const MIN_PASSPHRASE_LENGTH = 8;
const PBKDF2_ITERATIONS = 210_000;
const SALT_BYTES = 16;
const IV_BYTES = 12;

export interface VaultEnvelope {
	v: number;
	kdf: 'pbkdf2-sha256';
	iterations: number;
	salt: string;
	iv: string;
	data: string;
}

export class VaultError extends Error {
	constructor(message: string) {
		super(message);
		this.name = 'VaultError';
	}
}

function subtle(): SubtleCrypto {
	const crypto = globalThis.crypto;
	if (!crypto?.subtle) throw new VaultError('WebCrypto is unavailable in this environment');
	return crypto.subtle;
}

function toBase64(bytes: Uint8Array): string {
	let binary = '';
	for (const byte of bytes) binary += String.fromCharCode(byte);
	return btoa(binary);
}

function fromBase64(text: string): Uint8Array {
	const binary = atob(text);
	const bytes = new Uint8Array(binary.length);
	for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
	return bytes;
}

export function validatePassphrase(passphrase: string): void {
	if (typeof passphrase !== 'string' || passphrase.length < MIN_PASSPHRASE_LENGTH) {
		throw new VaultError(`Passphrase must be at least ${MIN_PASSPHRASE_LENGTH} characters`);
	}
}

export function isVaultEnvelope(value: unknown): value is VaultEnvelope {
	if (!value || typeof value !== 'object') return false;
	const record = value as Record<string, unknown>;
	return (
		record.v === VAULT_VERSION &&
		record.kdf === 'pbkdf2-sha256' &&
		typeof record.iterations === 'number' &&
		typeof record.salt === 'string' &&
		typeof record.iv === 'string' &&
		typeof record.data === 'string'
	);
}

async function deriveKey(passphrase: string, salt: Uint8Array): Promise<CryptoKey> {
	const base = await subtle().importKey('raw', new TextEncoder().encode(passphrase), 'PBKDF2', false, [
		'deriveKey'
	]);
	return subtle().deriveKey(
		{ name: 'PBKDF2', salt: salt as BufferSource, iterations: PBKDF2_ITERATIONS, hash: 'SHA-256' },
		base,
		{ name: 'AES-GCM', length: 256 },
		false,
		['encrypt', 'decrypt']
	);
}

/** Encrypt `plaintext` under a fresh random salt; returns the envelope. */
export async function encryptSecrets(plaintext: string, passphrase: string): Promise<VaultEnvelope> {
	validatePassphrase(passphrase);
	const salt = globalThis.crypto.getRandomValues(new Uint8Array(SALT_BYTES));
	const iv = globalThis.crypto.getRandomValues(new Uint8Array(IV_BYTES));
	const key = await deriveKey(passphrase, salt);
	const ciphertext = await subtle().encrypt(
		{ name: 'AES-GCM', iv: iv as BufferSource },
		key,
		new TextEncoder().encode(plaintext)
	);
	return {
		v: VAULT_VERSION,
		kdf: 'pbkdf2-sha256',
		iterations: PBKDF2_ITERATIONS,
		salt: toBase64(salt),
		iv: toBase64(iv),
		data: toBase64(new Uint8Array(ciphertext))
	};
}

/** Encrypt with an already-derived session key (fast path for saves). */
export async function encryptSecretsWithKey(
	plaintext: string,
	key: CryptoKey,
	salt: Uint8Array
): Promise<VaultEnvelope> {
	const iv = globalThis.crypto.getRandomValues(new Uint8Array(IV_BYTES));
	const ciphertext = await subtle().encrypt(
		{ name: 'AES-GCM', iv: iv as BufferSource },
		key,
		new TextEncoder().encode(plaintext)
	);
	return {
		v: VAULT_VERSION,
		kdf: 'pbkdf2-sha256',
		iterations: PBKDF2_ITERATIONS,
		salt: toBase64(salt),
		iv: toBase64(iv),
		data: toBase64(new Uint8Array(ciphertext))
	};
}

/** Derive the session key for `passphrase` + the envelope's salt. */
export async function deriveSessionKey(
	passphrase: string,
	envelope: VaultEnvelope
): Promise<{ key: CryptoKey; salt: Uint8Array }> {
	validatePassphrase(passphrase);
	const salt = fromBase64(envelope.salt);
	return { key: await deriveKey(passphrase, salt), salt };
}

export interface VaultSession {
	key: CryptoKey;
	salt: Uint8Array;
}

/** Fresh session (random salt) for a newly-set passphrase. */
export async function createVaultSession(passphrase: string): Promise<VaultSession> {
	validatePassphrase(passphrase);
	const salt = globalThis.crypto.getRandomValues(new Uint8Array(SALT_BYTES));
	return { key: await deriveKey(passphrase, salt), salt };
}

/** Decrypt with an already-derived session key (single-derivation unlock). */
export async function decryptSecretsWithKey(
	envelope: VaultEnvelope,
	key: CryptoKey
): Promise<string> {
	try {
		const plaintext = await subtle().decrypt(
			{ name: 'AES-GCM', iv: fromBase64(envelope.iv) as BufferSource },
			key,
			fromBase64(envelope.data) as BufferSource
		);
		return new TextDecoder().decode(plaintext);
	} catch {
		throw new VaultError('Wrong passphrase or corrupted vault data');
	}
}

/**
 * Decrypt an envelope. Returns the plaintext, or throws `VaultError` when the
 * passphrase is wrong or the envelope is tampered with (AES-GCM auth).
 */
export async function decryptSecrets(envelope: VaultEnvelope, passphrase: string): Promise<string> {
	const { key } = await deriveSessionKey(passphrase, envelope);
	try {
		const plaintext = await subtle().decrypt(
			{ name: 'AES-GCM', iv: fromBase64(envelope.iv) as BufferSource },
			key,
			fromBase64(envelope.data) as BufferSource
		);
		return new TextDecoder().decode(plaintext);
	} catch (error) {
		if (error instanceof VaultError) throw error;
		throw new VaultError('Wrong passphrase or corrupted vault data');
	}
}
