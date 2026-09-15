---
aside: false
outline: false
layout: page
---

<script setup>
import { withBase } from "vitepress";
const iframeSrc = withBase("/api/asyncapi/index.html");
</script>

<iframe
  :src="iframeSrc"
  title="AsyncAPI reference"
  style="width: 100%; height: calc(100vh - 4rem); border: 0; background: var(--vp-c-bg);"
  loading="lazy"
></iframe>

<noscript>

# Message Bus (AsyncAPI)

The interactive reference requires JavaScript. The raw AsyncAPI JSON
is at [`/api/asyncapi.json`](/api/asyncapi.json), and the durable
human-readable reference is the architecture doc:
[`Architecture › Message Bus & Persistent Notifications`](../architecture/message-bus-and-notifications.md).

</noscript>
