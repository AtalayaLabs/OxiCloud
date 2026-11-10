# Theme dependencies

OxiCloud remains MIT-licensed. No CSS parser or theme-format dependency is needed:
operator-installed CSS is served unchanged and interpreted by the browser.

`rustix` (Apache-2.0 OR MIT) provides descriptor-relative filesystem operations.
`brotli` (BSD-3-Clause AND MIT), already used by HTTP compression, prepares the
stylesheet once at load time. Retain dependency licenses in binary distributions.
