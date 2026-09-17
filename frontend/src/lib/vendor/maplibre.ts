/**
 * Minimal typings + lazy loader for the vendored MapLibre GL + pmtiles globals.
 *
 * The libraries are heavy (~1 MB) and only the Places map needs them, so they
 * are vendored under `/vendors` (not bundled) and injected on first use — the
 * same pattern the legacy frontend used. We declare only the small slice of the
 * MapLibre API the Places view touches, so the rest of the app stays `any`-free.
 */

import { base } from '$app/paths';

export interface MapBounds {
	getWest(): number;
	getSouth(): number;
	getEast(): number;
	getNorth(): number;
}

export interface LngLatBounds {
	extend(lngLat: [number, number]): LngLatBounds;
	isEmpty(): boolean;
}

export interface MapMarker {
	setLngLat(lngLat: [number, number]): MapMarker;
	addTo(map: MapLibreMap): MapMarker;
	remove(): void;
}

export interface MapLibreMap {
	addControl(control: unknown, position?: string): MapLibreMap;
	on(type: string, listener: () => void): void;
	getBounds(): MapBounds;
	getZoom(): number;
	resize(): void;
	easeTo(opts: { center: [number, number]; zoom: number }): void;
	fitBounds(
		bounds: LngLatBounds,
		opts?: { padding?: number; maxZoom?: number; duration?: number }
	): void;
	remove(): void;
}

export interface MapLibreModule {
	Map: new (opts: Record<string, unknown>) => MapLibreMap;
	Marker: new (opts: { element: HTMLElement }) => MapMarker;
	NavigationControl: new (opts?: { showCompass?: boolean }) => unknown;
	AttributionControl: new (opts?: { customAttribution?: string }) => unknown;
	LngLatBounds: new () => LngLatBounds;
	addProtocol(name: string, fn: unknown): void;
}

export interface PMTilesModule {
	Protocol: new () => { tile: unknown };
}

export interface MapLibs {
	maplibregl: MapLibreModule;
	pmtiles: PMTilesModule;
}

let cached: MapLibs | null = null;

/** Inject a vendored script once, resolving when it has loaded. */
function loadScript(src: string): Promise<void> {
	return new Promise((resolve, reject) => {
		if (document.querySelector(`script[data-vendor="${src}"]`)) {
			resolve();
			return;
		}
		const s = document.createElement('script');
		s.src = src;
		s.async = true;
		s.dataset.vendor = src;
		s.addEventListener('load', () => resolve());
		s.addEventListener('error', () => reject(new Error(`Failed to load ${src}`)));
		document.head.appendChild(s);
	});
}

/** Lazy-load MapLibre GL + pmtiles.js (+ MapLibre CSS) and read their globals. */
export async function loadMapLibs(): Promise<MapLibs> {
	if (cached) return cached;
	if (!document.querySelector('link[data-vendor="maplibre-css"]')) {
		const l = document.createElement('link');
		l.rel = 'stylesheet';
		l.href = `${base}/vendors/maplibre-gl.css`;
		l.dataset.vendor = 'maplibre-css';
		document.head.appendChild(l);
	}
	await loadScript(`${base}/vendors/maplibre-gl.js`);
	await loadScript(`${base}/vendors/pmtiles.js`);
	const w = window as unknown as { maplibregl?: MapLibreModule; pmtiles?: PMTilesModule };
	if (!w.maplibregl || !w.pmtiles) throw new Error('map libraries failed to initialise');
	cached = { maplibregl: w.maplibregl, pmtiles: w.pmtiles };
	return cached;
}
