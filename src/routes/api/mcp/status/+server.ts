import { json, type RequestHandler } from '@sveltejs/kit';
import { env } from '$env/dynamic/private';
import { isMcpProxyEnabled } from '../shared.ts';

export const GET: RequestHandler = async () => {
	return json({ enabled: isMcpProxyEnabled(env.MCP_ENABLED) });
};
