import { describe, it, expect } from 'vitest';
import { renderOpsBannerMarkdown } from './opsBannerMarkdown';

describe('renderOpsBannerMarkdown', () => {
	it('escapes raw HTML', () => {
		expect(renderOpsBannerMarkdown('<script>alert(1)</script>')).toBe(
			'&lt;script&gt;alert(1)&lt;/script&gt;'
		);
	});

	it('renders bold', () => {
		expect(renderOpsBannerMarkdown('**scheduled**')).toBe('<strong>scheduled</strong>');
	});

	it('renders italic with * and _', () => {
		expect(renderOpsBannerMarkdown('*maintenance*')).toBe('<em>maintenance</em>');
		expect(renderOpsBannerMarkdown('_maintenance_')).toBe('<em>maintenance</em>');
	});

	it('renders inline code', () => {
		expect(renderOpsBannerMarkdown('run `cargo check`')).toBe('run <code>cargo check</code>');
	});

	it('bold inside code is not applied', () => {
		expect(renderOpsBannerMarkdown('`**literal**`')).toBe('<code>**literal**</code>');
	});

	it('renders safe https link', () => {
		expect(renderOpsBannerMarkdown('see [docs](https://example.com/x)')).toBe(
			'see <a href="https://example.com/x" target="_blank" rel="noopener noreferrer">docs</a>'
		);
	});

	it('renders mailto link', () => {
		expect(renderOpsBannerMarkdown('[contact](mailto:ops@example.com)')).toBe(
			'<a href="mailto:ops@example.com" target="_blank" rel="noopener noreferrer">contact</a>'
		);
	});

	it('refuses javascript: links', () => {
		const r = renderOpsBannerMarkdown('[x](javascript:alert(1))');
		expect(r).not.toContain('<a');
		expect(r).toContain('[x]');
		expect(r).toContain('javascript:');
	});

	it('refuses data: URIs', () => {
		const r = renderOpsBannerMarkdown('[x](data:text/html,<script>alert(1)</script>)');
		expect(r).not.toContain('<a');
	});

	it('refuses relative paths', () => {
		const r = renderOpsBannerMarkdown('[x](/admin)');
		expect(r).not.toContain('<a');
		expect(r).toContain('[x]');
	});

	it('converts line breaks', () => {
		expect(renderOpsBannerMarkdown('line one\nline two')).toBe('line one<br>line two');
	});

	it('collapses consecutive empty lines to single break', () => {
		expect(renderOpsBannerMarkdown('a\n\n\nb')).toBe('a<br><br>b');
	});

	it('trims trailing empty lines', () => {
		expect(renderOpsBannerMarkdown('a\n\n')).toBe('a');
	});

	it('does not render headings', () => {
		expect(renderOpsBannerMarkdown('# heading')).toBe('# heading');
	});

	it('does not render tables', () => {
		const r = renderOpsBannerMarkdown('| a | b |\n|---|---|');
		expect(r).not.toContain('<table');
	});

	it('escapes attribute-injection attempts in link label', () => {
		// The label text may contain the WORD onerror once escaped —
		// what matters is that it cannot escape the `<a>...</a>`
		// text-content context. Any quote that would close the
		// attribute (or break the content) gets escaped to `&quot;`
		// / `&#39;`, so an `onerror="...` substring stays as literal
		// text, not an event handler.
		const r = renderOpsBannerMarkdown('[" onerror="alert(1)](https://x.test)');
		// Quotes escaped — the only mechanism by which "onerror"
		// could become an attribute is a bare `"` that we did NOT
		// escape. Pin that no raw unescaped quote survives.
		expect(r).not.toMatch(/[^&]"[^>]*onerror/);
		expect(r).toContain('&quot;');
	});

	it('escapes link href special chars', () => {
		const r = renderOpsBannerMarkdown('[x](https://evil.test"</a><script>alert(1)</script>)');
		// The URL still passes the scheme check but special chars are escaped.
		expect(r).not.toContain('<script');
		expect(r).toContain('&lt;');
	});

	it('empty input returns empty string', () => {
		expect(renderOpsBannerMarkdown('')).toBe('');
	});
});
