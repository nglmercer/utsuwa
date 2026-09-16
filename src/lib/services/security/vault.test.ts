import test from 'node:test';
import assert from 'node:assert/strict';

import {
	decryptSecrets,
	deriveSessionKey,
	encryptSecrets,
	encryptSecretsWithKey,
	isVaultEnvelope,
	MIN_PASSPHRASE_LENGTH,
	validatePassphrase,
	VaultError
} from './vault.ts';

const PASSPHRASE = 'correct horse battery staple';

test('round-trip preserves secrets and randomizes envelopes', async () => {
	const plaintext = JSON.stringify({ openai: { apiKey: 'sk-test' } });
	const first = await encryptSecrets(plaintext, PASSPHRASE);
	const second = await encryptSecrets(plaintext, PASSPHRASE);
	assert.equal(await decryptSecrets(first, PASSPHRASE), plaintext);
	assert.notEqual(first.iv, second.iv);
	assert.notEqual(first.salt, second.salt);
	assert.notEqual(first.data, second.data);
	assert.ok(isVaultEnvelope(first));
});

test('wrong passphrase and tampering fail closed', async () => {
	const envelope = await encryptSecrets('{"a":1}', PASSPHRASE);
	await assert.rejects(decryptSecrets(envelope, 'wrong passphrase here'), VaultError);
	const tampered = { ...envelope, data: `${envelope.data.slice(0, -4)}AAAA` };
	await assert.rejects(decryptSecrets(tampered, PASSPHRASE), VaultError);
});

test('session-key fast path decrypts with the original passphrase', async () => {
	const first = await encryptSecrets('{"a":1}', PASSPHRASE);
	const { key, salt } = await deriveSessionKey(PASSPHRASE, first);
	const resaved = await encryptSecretsWithKey('{"a":2}', key, salt);
	assert.equal(await decryptSecrets(resaved, PASSPHRASE), '{"a":2}');
});

test('short passphrases are rejected and envelopes are validated', () => {
	assert.throws(() => validatePassphrase('short'), VaultError);
	assert.throws(() => validatePassphrase(''), VaultError);
	assert.equal(isVaultEnvelope(null), false);
	assert.equal(isVaultEnvelope({}), false);
	assert.equal(isVaultEnvelope({ v: 999, kdf: 'pbkdf2-sha256' }), false);
	assert.ok(MIN_PASSPHRASE_LENGTH >= 8);
});
