// Puts a file where users expect downloads to land via a normal browser
// download. (The removed desktop backend used to write straight into the
// Downloads folder; if blob downloads prove unreliable in the native
// webview, route this through the host IPC bridge instead.) Resolves
// false: the browser always takes over.
export async function saveToDownloads(filename: string, blob: Blob): Promise<boolean> {
	const url = URL.createObjectURL(blob);
	const a = document.createElement('a');
	a.href = url;
	a.download = filename;
	a.click();
	URL.revokeObjectURL(url);
	return false;
}
