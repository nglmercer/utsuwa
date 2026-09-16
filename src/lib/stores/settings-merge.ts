// Pure merge for vault unlock: locked-time in-memory edits are newer than the
// stored envelope, so they overlay it instead of being discarded.
// Framework-free so it runs under `node --test`.

export interface SecretSnapshot<P, S> {
	providerConfigs: Record<string, P>;
	mcpServers: S[];
}

/** Overlay pending locked-time edits onto freshly decrypted vault secrets.
 * In-memory (newer) wins per provider and per server id; unknown in-memory
 * servers are appended. Locked-time *deletions* cannot be represented and
 * are lost — the UI disables secret editing while locked, so only
 * programmatic writers (e.g. native re-hydration) hit this path. */
export function mergePendingSecrets<P, S extends { id: string }>(
	decrypted: SecretSnapshot<P, S>,
	pending: SecretSnapshot<P, S>
): SecretSnapshot<P, S> {
	const pendingById = new Map(pending.mcpServers.map((server) => [server.id, server]));
	const decryptedIds = new Set(decrypted.mcpServers.map((server) => server.id));
	return {
		providerConfigs: { ...decrypted.providerConfigs, ...pending.providerConfigs },
		mcpServers: [
			...decrypted.mcpServers.map((server) => pendingById.get(server.id) ?? server),
			...pending.mcpServers.filter((server) => !decryptedIds.has(server.id))
		]
	};
}
