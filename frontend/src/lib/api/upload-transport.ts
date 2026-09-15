/** XHR wire adapter for the generated client: fetch has no upload progress events. */
export function uploadTransport(
	form: FormData,
	onProgress: (fraction: number) => void
): typeof fetch {
	return async (input, init) => {
		const request = new Request(
			typeof input === 'string' ? new URL(input, location.origin) : input,
			init
		);
		// A token refresh uses the regular wire transport, never the upload body.
		if (new URL(request.url).pathname !== '/api/files/upload') return fetch(request);
		return new Promise<Response>((resolve, reject) => {
			const xhr = new XMLHttpRequest();
			xhr.open(request.method, request.url);
			xhr.withCredentials = request.credentials !== 'omit';
			request.headers.forEach((value, name) => {
				// XHR serializes the original FormData and chooses its own boundary.
				if (name.toLowerCase() !== 'content-type') xhr.setRequestHeader(name, value);
			});
			const SEND_STALL_MS = 30_000;
			const RESPONSE_MS = 60_000;
			let watchdog: ReturnType<typeof setTimeout>;
			const cleanup = () => {
				clearTimeout(watchdog);
				request.signal.removeEventListener('abort', abort);
			};
			const abort = () => xhr.abort();
			const arm = (ms: number) => {
				clearTimeout(watchdog);
				watchdog = setTimeout(abort, ms);
			};
			xhr.upload.onprogress = (event) => {
				onProgress(event.lengthComputable ? event.loaded / event.total : NaN);
				arm(SEND_STALL_MS);
			};
			xhr.upload.onload = () => arm(RESPONSE_MS);
			xhr.onload = () => {
				cleanup();
				const headers = new Headers();
				for (const name of ['Content-Type', 'DPoP-Nonce', 'WWW-Authenticate', 'X-Server-Status']) {
					const value = xhr.getResponseHeader(name);
					if (value) headers.set(name, value);
				}
				resolve(
					new Response(xhr.status === 204 ? null : (xhr.responseText ?? ''), {
						status: xhr.status,
						headers
					})
				);
			};
			xhr.onerror = () => {
				cleanup();
				reject(new Error('upload failed: network error'));
			};
			xhr.onabort = () => {
				cleanup();
				reject(new Error('upload stalled — aborted'));
			};
			if (request.signal.aborted) {
				reject(request.signal.reason);
				return;
			}
			request.signal.addEventListener('abort', abort, { once: true });
			arm(SEND_STALL_MS);
			xhr.send(form);
		});
	};
}
