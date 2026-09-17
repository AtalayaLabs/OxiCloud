<script lang="ts">
	import { base } from '$app/paths';
	/**
	 * Places: geotagged photos on a self-hosted MapLibre GL map. Clusters are
	 * computed server-side (`GET /api/photos/geo`), so we draw one lightweight HTML
	 * marker per cluster — no glyphs/sprites, no client-side clustering. The vector
	 * basemap is optional: if `/basemaps/basemap.pmtiles` is present it is read over
	 * HTTP Range (pmtiles.js); otherwise the map falls back to a themed background
	 * and still shows the clusters.
	 */
	import PhotoLightbox from '$lib/components/PhotoLightbox.svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import { fetchPhotosGeo, type GeoCluster } from '$lib/api/endpoints/photos';
	import { fileThumbnailUrl } from '$lib/api/endpoints/files';
	import type { FileItem } from '$lib/api/types';
	import { t } from '$lib/i18n/index.svelte';
	import { minimalPhotoItem } from '$lib/utils/media';
	import {
		loadMapLibs,
		type LngLatBounds,
		type MapLibreMap,
		type MapLibs,
		type MapMarker
	} from '$lib/vendor/maplibre';
	import { onDestroy, onMount } from 'svelte';

	const BASEMAP_URL = `${base}/basemaps/basemap.pmtiles`;
	// Bundled lightweight world outline (Natural Earth 110m, public domain),
	// shown when no Protomaps .pmtiles basemap is installed so Places is never a
	// blank background. Same-origin asset — no external tiles (CSP-friendly).
	const WORLD_GEOJSON_URL = `${base}/geo/world-110m.geojson`;

	let mapEl = $state<HTMLDivElement | null>(null);
	let loading = $state(true);
	let error = $state(false);

	let libs: MapLibs | null = null;
	let map: MapLibreMap | null = null;
	let markers: MapMarker[] = [];
	let moveTimer = 0;
	let hasBasemap: boolean | null = null;

	// Lightbox drill-in (single representative photo).
	let lbItems = $state<FileItem[]>([]);
	let lbIndex = $state(-1);

	function isDark(): boolean {
		const attr = document.documentElement.getAttribute('data-color-scheme');
		if (attr === 'dark') return true;
		if (attr === 'light') return false;
		return window.matchMedia?.('(prefers-color-scheme: dark)').matches ?? false;
	}

	/** Whether a basemap .pmtiles is available (cached after first probe). */
	async function checkBasemap(): Promise<boolean> {
		if (hasBasemap !== null) return hasBasemap;
		try {
			const res = await fetch(BASEMAP_URL, { headers: { Range: 'bytes=0-0' } });
			// The SPA fallback serves index.html (HTTP 200, text/html) for any
			// missing path, so `res.ok` alone can't distinguish "basemap present"
			// from "absent". A real .pmtiles is binary (octet-stream); the shell
			// is HTML — treat an HTML body as "no basemap" and fall back cleanly.
			const type = res.headers.get('Content-Type') ?? '';
			hasBasemap = res.ok && !type.toLowerCase().includes('text/html');
		} catch {
			hasBasemap = false;
		}
		return hasBasemap;
	}

	/**
	 * Fallback basemap used when no Protomaps `.pmtiles` is installed: a themed
	 * ocean with land masses and country borders drawn from the bundled Natural
	 * Earth outline (`WORLD_GEOJSON_URL`). Gives Places a recognisable world map
	 * — no street/label detail — without any per-instance basemap install.
	 */
	function worldStyle(): Record<string, unknown> {
		const c = isDark()
			? { ocean: '#0f172a', land: '#1f2a3a', border: '#3d4a5c' }
			: { ocean: '#bcd4ea', land: '#eef1ee', border: '#aab6c4' };
		return {
			version: 8,
			sources: {
				world: { type: 'geojson', data: WORLD_GEOJSON_URL }
			},
			layers: [
				{ id: 'ocean', type: 'background', paint: { 'background-color': c.ocean } },
				{ id: 'land', type: 'fill', source: 'world', paint: { 'fill-color': c.land } },
				{
					id: 'borders',
					type: 'line',
					source: 'world',
					paint: { 'line-color': c.border, 'line-width': 0.6 }
				}
			]
		};
	}

	/** Label-light Protomaps vector style (no glyphs/sprites required). */
	function basemapStyle(): Record<string, unknown> {
		const dark = isDark();
		const c = dark
			? {
					earth: '#1b2433',
					land: '#222d3d',
					water: '#0d1b2a',
					roads: '#3a4860',
					buildings: '#2a3547',
					boundary: '#475569'
				}
			: {
					earth: '#f3efe9',
					land: '#e9e4da',
					water: '#a8c8e8',
					roads: '#ffffff',
					buildings: '#e0dccf',
					boundary: '#c9c2b6'
				};
		return {
			version: 8,
			sources: {
				protomaps: {
					type: 'vector',
					url: `pmtiles://${BASEMAP_URL}`,
					attribution: 'Protomaps © OpenStreetMap'
				}
			},
			layers: [
				{ id: 'bg', type: 'background', paint: { 'background-color': c.earth } },
				{
					id: 'earth',
					type: 'fill',
					source: 'protomaps',
					'source-layer': 'earth',
					paint: { 'fill-color': c.earth }
				},
				{
					id: 'landuse',
					type: 'fill',
					source: 'protomaps',
					'source-layer': 'landuse',
					paint: { 'fill-color': c.land, 'fill-opacity': 0.6 }
				},
				{
					id: 'water',
					type: 'fill',
					source: 'protomaps',
					'source-layer': 'water',
					paint: { 'fill-color': c.water }
				},
				{
					id: 'roads',
					type: 'line',
					source: 'protomaps',
					'source-layer': 'roads',
					minzoom: 7,
					paint: { 'line-color': c.roads, 'line-width': 0.8 }
				},
				{
					id: 'buildings',
					type: 'fill',
					source: 'protomaps',
					'source-layer': 'buildings',
					minzoom: 13,
					paint: { 'fill-color': c.buildings }
				},
				{
					id: 'boundaries',
					type: 'line',
					source: 'protomaps',
					'source-layer': 'boundaries',
					paint: { 'line-color': c.boundary, 'line-width': 0.6, 'line-dasharray': [2, 2] }
				}
			]
		};
	}

	async function initMap() {
		if (!mapEl) return;
		try {
			libs = await loadMapLibs();
		} catch {
			error = true;
			loading = false;
			return;
		}
		const { maplibregl, pmtiles } = libs;
		const basemap = await checkBasemap();
		if (basemap) {
			try {
				const protocol = new pmtiles.Protocol();
				maplibregl.addProtocol('pmtiles', protocol.tile);
			} catch {
				/* fall through to a basemap-less map */
			}
		}

		map = new maplibregl.Map({
			container: mapEl,
			style: basemap ? basemapStyle() : worldStyle(),
			center: [0, 25],
			zoom: 1.3,
			attributionControl: false
		});
		map.addControl(new maplibregl.NavigationControl({ showCompass: false }), 'top-right');
		if (basemap) {
			map.addControl(
				new maplibregl.AttributionControl({
					customAttribution:
						'Protomaps © <a href="https://www.openstreetmap.org/copyright" target="_blank" rel="noopener">OpenStreetMap</a>'
				})
			);
		}

		map.on('load', () => {
			loading = false;
			void refreshClusters(true);
		});
		map.on('moveend', () => {
			clearTimeout(moveTimer);
			moveTimer = window.setTimeout(() => void refreshClusters(false), 250);
		});
		// Tolerate a missing/misdeployed world outline quietly: the SPA fallback
		// serves index.html (HTTP 200, text/html) for an absent /geo asset, which
		// MapLibre can't parse as GeoJSON. Markers still render over the ocean
		// layer, so swallow the source error instead of logging to the console.
		map.on('error', () => {});
	}

	/** Fetch clusters for the current viewport and render them.
	 * @param fit Fit the map to the returned clusters (first load only). */
	async function refreshClusters(fit: boolean) {
		if (!map) return;
		const b = map.getBounds();
		const bbox = `${b.getWest()},${b.getSouth()},${b.getEast()},${b.getNorth()}`;
		const zoom = Math.round(map.getZoom());
		try {
			const clusters = await fetchPhotosGeo(bbox, zoom);
			renderMarkers(clusters);
			if (fit && clusters.length) fitTo(clusters);
		} catch {
			/* transient geo fetch failure — leave the current markers in place */
		}
	}

	function renderMarkers(clusters: GeoCluster[]) {
		for (const m of markers) m.remove();
		markers = [];
		if (!libs || !map) return;
		const { maplibregl } = libs;
		for (const c of clusters) {
			const size = Math.round(Math.min(64, 30 + Math.log2(c.count + 1) * 6));
			const el = document.createElement('div');
			el.className = 'places-cluster';
			el.style.width = `${size}px`;
			el.style.height = `${size}px`;
			el.style.backgroundImage = `url(${fileThumbnailUrl(c.sample_file_id, 'icon')})`;
			if (c.count > 1) {
				const count = document.createElement('span');
				count.className = 'places-cluster__count';
				count.textContent = String(c.count);
				el.appendChild(count);
			}
			el.addEventListener('click', () => onClusterClick(c));
			markers.push(new maplibregl.Marker({ element: el }).setLngLat([c.lng, c.lat]).addTo(map));
		}
	}

	function onClusterClick(c: GeoCluster) {
		if (!map) return;
		const zoom = map.getZoom();
		if (c.count === 1 || zoom >= 16) {
			lbItems = [minimalPhotoItem(c.sample_file_id)];
			lbIndex = 0;
		} else {
			map.easeTo({ center: [c.lng, c.lat], zoom: Math.min(zoom + 2.5, 17) });
		}
	}

	function fitTo(clusters: GeoCluster[]) {
		if (!libs || !map) return;
		const bounds: LngLatBounds = new libs.maplibregl.LngLatBounds();
		for (const c of clusters) bounds.extend([c.lng, c.lat]);
		if (!bounds.isEmpty()) map.fitBounds(bounds, { padding: 64, maxZoom: 14, duration: 0 });
	}

	onMount(initMap);

	onDestroy(() => {
		clearTimeout(moveTimer);
		for (const m of markers) m.remove();
		markers = [];
		map?.remove();
		map = null;
	});
</script>

<div class="places">
	<div class="places__map" bind:this={mapEl}></div>
	{#if loading && !error}
		<div class="places__loading"><Icon name="spinner" /></div>
	{/if}
	{#if error}
		<div class="places__error">{t('photos.map_error', 'Could not load the map')}</div>
	{/if}
</div>

<PhotoLightbox items={lbItems} bind:index={lbIndex} />

<style>
	.places {
		position: relative;
		height: calc(100vh - 8rem);
		min-height: 24rem;
	}

	.places__map {
		position: absolute;
		inset: 0;
	}

	.places__loading,
	.places__error {
		position: absolute;
		inset: 0;
		display: grid;
		place-items: center;
		color: var(--color-text-muted);
		pointer-events: none;
	}

	.places__loading :global(svg) {
		animation: places-spin 1s linear infinite;
		font-size: 1.5rem;
	}

	@keyframes places-spin {
		to {
			transform: rotate(360deg);
		}
	}

	/* Cluster markers are created imperatively by MapLibre, outside Svelte's
	   scoped styles — hence :global. */
	:global(.places-cluster) {
		position: relative;
		border-radius: 50%;
		background-size: cover;
		background-position: center;
		border: 2px solid var(--color-on-accent);
		box-shadow: 0 1px 4px var(--color-overlay-shadow);
		cursor: pointer;
	}

	:global(.places-cluster__count) {
		position: absolute;
		top: -6px;
		right: -6px;
		min-width: 18px;
		height: 18px;
		padding: 0 4px;
		border-radius: 9px;
		background: var(--color-accent);
		color: var(--color-on-accent);
		font-size: 11px;
		font-weight: var(--weight-bold);
		display: grid;
		place-items: center;
	}
</style>
